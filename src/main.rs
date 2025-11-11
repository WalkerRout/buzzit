#![no_std]
#![no_main]
#![feature(impl_trait_in_assoc_type)]

use core::future;

use embassy_executor::Spawner;
use embassy_net::{Runner, StackResources};
use embassy_sync::{blocking_mutex::raw::CriticalSectionRawMutex, watch::Watch};
use embassy_time::{Duration, Instant, Timer};

use esp_alloc as _;
use esp_backtrace as _;
use esp_hal::clock::CpuClock;
use esp_hal::gpio::{Level, Output, OutputConfig};
#[cfg(target_arch = "riscv32")]
use esp_hal::interrupt::software::SoftwareInterruptControl;
use esp_hal::ram;
use esp_hal::rng::Rng;
use esp_hal::timer::timg::TimerGroup;
use esp_println::println;
use esp_radio::wifi::{
  self, ClientConfig, ModeConfig, WifiController, WifiDevice, WifiEvent, WifiStaState,
};
use esp_radio::Controller;

use picoserve::response::ws::{Message, SocketRx, SocketTx, WebSocketCallback};
use picoserve::response::{Json, WebSocketUpgrade};
use picoserve::routing::{get, PathRouter};
use picoserve::{make_static, AppBuilder, AppRouter, Config, Router, Server};

esp_bootloader_esp_idf::esp_app_desc!();

macro_rules! mk_static {
  ($t:ty,$val:expr) => {{
    static STATIC_CELL: static_cell::StaticCell<$t> = static_cell::StaticCell::new();
    #[deny(unused_attributes)]
    let x = STATIC_CELL.uninit().write(($val));
    x
  }};
}

#[derive(serde::Serialize)]
struct StatusResponse {
  status: &'static str,
}

struct AppProps;

impl AppBuilder for AppProps {
  type PathRouter = impl PathRouter;

  fn build_app(self) -> Router<Self::PathRouter> {
    Router::new()
      .route(
        "/buzz",
        get(async |upgrade: WebSocketUpgrade| upgrade.on_upgrade(BuzzerWebSocket)),
      )
      .route(
        "/status",
        get(|| async move { Json(StatusResponse { status: "ok" }) }),
      )
  }
}

#[derive(Clone, Copy)]
enum BuzzerState {
  Off,
  On(Instant), // timestamp of last beat
}

static BUZZER_STATE: Watch<CriticalSectionRawMutex, BuzzerState, 2> =
  Watch::new_with(BuzzerState::Off);

struct BuzzerWebSocket;

impl WebSocketCallback for BuzzerWebSocket {
  async fn run<R: embedded_io_async::Read, W: embedded_io_async::Write<Error = R::Error>>(
    self,
    mut rx: SocketRx<R>,
    mut tx: SocketTx<W>,
  ) -> Result<(), W::Error> {
    println!("websocket connected");
    let mut buffer = [0; 128];

    let close_reason = loop {
      match rx
        .next_message(&mut buffer, future::pending())
        .await?
        .ignore_never_b()
      {
        Ok(Message::Ping(data)) => {
          BUZZER_STATE.sender().send(BuzzerState::On(Instant::now()));
          tx.send_pong(data).await?;
        }
        Ok(Message::Close(reason)) => {
          println!("websocket close - {reason:?}");
          break None;
        }
        Ok(Message::Pong(_)) => continue,
        Err(error) => {
          println!("websocket error - {error:?}");
          break Some((error.code(), "websocket error"));
        }
        _ => {
          println!("got message other than heartbeat...");
          break Some((1003, "expected ping frames only"));
        }
      }
    };

    // connection closed, turn off buzzer
    BUZZER_STATE.sender().send(BuzzerState::Off);
    println!("websocket disconnected");

    tx.close(close_reason).await
  }
}

// beep pattern for lower pitch
const BEEP_ON_MS: u64 = 25;
const BEEP_OFF_MS: u64 = 25;
// stop buzzing if no message for this duration
const MESSAGE_TIMEOUT: Duration = Duration::from_millis(500);

#[embassy_executor::task]
async fn buzzer_task(mut buzzer: Output<'static>) -> ! {
  let mut receiver = BUZZER_STATE.receiver().unwrap();
  let mut is_buzzing = false;

  loop {
    let state = receiver.get().await;

    match state {
      BuzzerState::Off => {
        if is_buzzing {
          println!("buzzer OFF");
          is_buzzing = false;
          buzzer.set_low();
        }
        // wait for state change
        receiver.changed().await;
      }
      BuzzerState::On(last_msg_time) => {
        // see if message timeout expired
        if Instant::now().duration_since(last_msg_time) > MESSAGE_TIMEOUT {
          println!("buzzer OFF (timeout)");
          BUZZER_STATE.sender().send(BuzzerState::Off);
          is_buzzing = false;
          buzzer.set_low();
        } else {
          if !is_buzzing {
            println!("buzzer ON");
            is_buzzing = true;
          }
          // generate beep pattern
          buzzer.set_high();
          Timer::after(Duration::from_millis(BEEP_ON_MS)).await;
          buzzer.set_low();
          Timer::after(Duration::from_millis(BEEP_OFF_MS)).await;
        }
      }
    }
  }
}

#[embassy_executor::task]
async fn net_task(mut runner: Runner<'static, WifiDevice<'static>>) {
  runner.run().await
}

const SSID: &str = env!("SSID");
const PASSWORD: &str = env!("PASSWORD");

#[embassy_executor::task]
async fn connection(mut controller: WifiController<'static>) {
  println!("starting connection task");
  loop {
    match wifi::sta_state() {
      WifiStaState::Connected => {
        controller.wait_for_event(WifiEvent::StaDisconnected).await;
        Timer::after(Duration::from_millis(5000)).await
      }
      _ => {}
    }

    if !matches!(controller.is_started(), Ok(true)) {
      let client_config = ModeConfig::Client(
        ClientConfig::default()
          .with_ssid(SSID.into())
          .with_password(PASSWORD.into()),
      );
      controller.set_config(&client_config).unwrap();
      println!("starting wifi");
      controller.start_async().await.unwrap();
      println!("wifi started!");
    }
    println!("connecting to wifi...");

    match controller.connect_async().await {
      Ok(_) => println!("wifi connected!"),
      Err(e) => {
        println!("failed to connect - {e}");
        Timer::after(Duration::from_millis(5000)).await
      }
    }
  }
}

const WEB_TASK_POOL_SIZE: usize = 2;

#[embassy_executor::task(pool_size = WEB_TASK_POOL_SIZE)]
async fn web_task(
  task_id: usize,
  stack: embassy_net::Stack<'static>,
  app: &'static AppRouter<AppProps>,
  config: &'static Config<Duration>,
) -> ! {
  let port = 80;
  let mut tcp_rx_buffer = [0; 1024];
  let mut tcp_tx_buffer = [0; 1024];
  let mut http_buffer = [0; 2048];

  // wait for network to be ready
  loop {
    if stack.is_link_up() {
      break;
    }
    Timer::after(Duration::from_millis(500)).await;
  }

  loop {
    if let Some(config) = stack.config_v4() {
      println!("server task #{task_id} listening at {}", config.address);
      break;
    }
    Timer::after(Duration::from_millis(500)).await;
  }

  loop {
    Server::new(app, config, &mut http_buffer)
      .listen_and_serve(task_id, stack, port, &mut tcp_rx_buffer, &mut tcp_tx_buffer)
      .await
      .into_never()
  }
}

#[esp_rtos::main]
async fn main(spawner: Spawner) -> ! {
  esp_println::logger::init_logger_from_env();
  let config = esp_hal::Config::default().with_cpu_clock(CpuClock::max());
  let peripherals = esp_hal::init(config);

  esp_alloc::heap_allocator!(#[ram(reclaimed)] size: 64 * 1024);
  esp_alloc::heap_allocator!(size: 36 * 1024);

  let buzzer = Output::new(peripherals.GPIO5, Level::Low, OutputConfig::default());

  let timg0 = TimerGroup::new(peripherals.TIMG0);
  #[cfg(target_arch = "riscv32")]
  let sw_int = SoftwareInterruptControl::new(peripherals.SW_INTERRUPT);
  esp_rtos::start(
    timg0.timer0,
    #[cfg(target_arch = "riscv32")]
    sw_int.software_interrupt0,
  );

  let esp_radio_ctrl = &*mk_static!(Controller<'static>, esp_radio::init().unwrap());

  let (controller, interfaces) =
    esp_radio::wifi::new(&esp_radio_ctrl, peripherals.WIFI, Default::default()).unwrap();

  let wifi_interface = interfaces.sta;

  let config = embassy_net::Config::dhcpv4(Default::default());

  let rng = Rng::new();
  let seed = (rng.random() as u64) << 32 | rng.random() as u64;

  let (stack, runner) = embassy_net::new(
    wifi_interface,
    config,
    mk_static!(StackResources<3>, StackResources::<3>::new()),
    seed,
  );

  spawner.spawn(net_task(runner)).ok();
  spawner.spawn(connection(controller)).ok();
  spawner.spawn(buzzer_task(buzzer)).ok();

  let app = make_static!(AppRouter<AppProps>, AppProps.build_app());

  let picoserve_config = make_static!(
    picoserve::Config::<Duration>,
    picoserve::Config::new(picoserve::Timeouts {
      start_read_request: Some(Duration::from_secs(5)),
      persistent_start_read_request: Some(Duration::from_secs(1)),
      read_request: Some(Duration::from_secs(1)),
      write: Some(Duration::from_secs(1)),
    })
    .keep_connection_alive()
  );

  for task_id in 0..WEB_TASK_POOL_SIZE {
    spawner
      .spawn(web_task(task_id, stack, app, picoserve_config))
      .ok();
  }

  loop {
    Timer::after(Duration::from_secs(1)).await;
  }
}