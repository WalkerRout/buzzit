#![no_std]
#![no_main]
#![feature(impl_trait_in_assoc_type)]

use core::future;
use core::sync::atomic::{AtomicU32, Ordering};

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

const MAX_CONCURRENT_CONNECTIONS: u32 = 4;

static ACTIVE_CONNECTIONS: AtomicU32 = AtomicU32::new(0);
static TOTAL_CONNECTIONS: AtomicU32 = AtomicU32::new(0);
static REJECTED_CONNECTIONS: AtomicU32 = AtomicU32::new(0);

#[derive(serde::Serialize)]
struct StatusResponse {
  status: &'static str,
  active_connections: u32,
  total_connections: u32,
  rejected_connections: u32,
  free_heap: u32,
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
        get(|| async move {
          Json(StatusResponse {
            status: "ok",
            active_connections: ACTIVE_CONNECTIONS.load(Ordering::Relaxed),
            total_connections: TOTAL_CONNECTIONS.load(Ordering::Relaxed),
            rejected_connections: REJECTED_CONNECTIONS.load(Ordering::Relaxed),
            free_heap: esp_alloc::HEAP.free() as u32,
          })
        }),
      )
  }
}

#[derive(Clone, Copy)]
enum BuzzerState {
  Off,
  On(Instant),
}

static BUZZER_STATE: Watch<CriticalSectionRawMutex, BuzzerState, 2> =
  Watch::new_with(BuzzerState::Off);

struct BuzzerWebSocket;

impl Drop for BuzzerWebSocket {
  fn drop(&mut self) {
    ACTIVE_CONNECTIONS.fetch_sub(1, Ordering::SeqCst);
  }
}

impl WebSocketCallback for BuzzerWebSocket {
  async fn run<R: embedded_io_async::Read, W: embedded_io_async::Write<Error = R::Error>>(
    self,
    mut rx: SocketRx<R>,
    mut tx: SocketTx<W>,
  ) -> Result<(), W::Error> {
    let current = ACTIVE_CONNECTIONS.load(Ordering::SeqCst);
    
    if current >= MAX_CONCURRENT_CONNECTIONS {
      REJECTED_CONNECTIONS.fetch_add(1, Ordering::Relaxed);
      println!("[WS] REJECTED: limit {}/{}", current, MAX_CONCURRENT_CONNECTIONS);
      return tx.close(Some((1008, "server at capacity"))).await;
    }
    
    ACTIVE_CONNECTIONS.fetch_add(1, Ordering::SeqCst);
    TOTAL_CONNECTIONS.fetch_add(1, Ordering::Relaxed);
    println!("[WS] connected (active: {})", current + 1);
    
    let mut buffer = [0; 128];
    let mut msg_count = 0u32;
    let mut last_log = Instant::now();
    let connect_time = Instant::now();
    let mut last_ping = Instant::now();
    let mut last_activity = Instant::now();

    let close_reason = loop {
      if Instant::now().duration_since(last_activity) > Duration::from_secs(10) {
        println!("[WS] TIMEOUT: idle >10s");
        break Some((1000, "idle timeout"));
      }
      
      let free_heap = esp_alloc::HEAP.free();
      if free_heap < 8192 {
        println!("[WS] ERROR: low heap ({} bytes)", free_heap);
        break Some((1011, "server overload"));
      }
      
      match rx.next_message(&mut buffer, future::pending()).await?.ignore_never_b() {
        Ok(Message::Ping(data)) => {
          last_activity = Instant::now();
          let now = Instant::now();
          
          if now.duration_since(last_ping) < Duration::from_millis(5) {
            continue;
          }
          last_ping = now;
          msg_count += 1;
          
          if now.duration_since(last_log) > Duration::from_secs(5) {
            println!(
              "[WS] stats: {} pings, uptime {}s, heap {}kb",
              msg_count,
              now.duration_since(connect_time).as_secs(),
              free_heap / 1024
            );
            last_log = now;
          }
          
          BUZZER_STATE.sender().send(BuzzerState::On(Instant::now()));
          
          if let Err(e) = tx.send_pong(data).await {
            println!("[WS] ERROR: pong failed - {:?}", e);
            break Some((1011, "pong send failed"));
          }
        }
        Ok(Message::Close(reason)) => {
          println!("[WS] close from client - {reason:?}");
          break None;
        }
        Ok(Message::Pong(_)) => {
          last_activity = Instant::now();
        }
        Err(error) => {
          println!("[WS] ERROR: protocol - {error:?}");
          break Some((error.code(), "websocket error"));
        }
        _ => {}
      }
    };

    BUZZER_STATE.sender().send(BuzzerState::Off);
    println!(
      "[WS] disconnected: {} pings, {}s",
      msg_count,
      Instant::now().duration_since(connect_time).as_secs()
    );

    tx.close(close_reason).await
  }
}

#[embassy_executor::task]
async fn buzzer_task(mut buzzer: Output<'static>) -> ! {
  let mut receiver = BUZZER_STATE.receiver().unwrap();
  let mut is_buzzing = false;

  loop {
    let state = receiver.get().await;

    match state {
      BuzzerState::Off => {
        if is_buzzing {
          is_buzzing = false;
          buzzer.set_low();
        }
        receiver.changed().await;
      }
      BuzzerState::On(last_msg_time) => {
        if Instant::now().duration_since(last_msg_time) > Duration::from_millis(500) {
          BUZZER_STATE.sender().send(BuzzerState::Off);
          is_buzzing = false;
          buzzer.set_low();
        } else {
          if !is_buzzing {
            is_buzzing = true;
          }
          buzzer.set_high();
          Timer::after(Duration::from_millis(25)).await;
          buzzer.set_low();
          Timer::after(Duration::from_millis(25)).await;
        }
      }
    }
  }
}

#[embassy_executor::task]
async fn watchdog_task() -> ! {
  loop {
    Timer::after(Duration::from_secs(10)).await;
    
    let free_heap = esp_alloc::HEAP.free();
    let active = ACTIVE_CONNECTIONS.load(Ordering::Relaxed);
    let total = TOTAL_CONNECTIONS.load(Ordering::Relaxed);
    let rejected = REJECTED_CONNECTIONS.load(Ordering::Relaxed);
    
    println!(
      "[WATCHDOG] heap: {}kb, conn: {}/{}, total: {}, reject: {}",
      free_heap / 1024, active, MAX_CONCURRENT_CONNECTIONS, total, rejected
    );
    
    if free_heap < 16384 {
      println!("[WATCHDOG] WARN: low heap");
    }
  }
}

#[embassy_executor::task]
async fn net_task(mut runner: Runner<'static, WifiDevice<'static>>) {
  runner.run().await
}

#[embassy_executor::task]
async fn connection(mut controller: WifiController<'static>) {
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
          .with_ssid(env!("SSID").into())
          .with_password(env!("PASSWORD").into()),
      );
      controller.set_config(&client_config).unwrap();
      controller.start_async().await.unwrap();
    }

    match controller.connect_async().await {
      Ok(_) => {}
      Err(_) => Timer::after(Duration::from_millis(5000)).await
    }
  }
}

#[embassy_executor::task(pool_size = 2)]
async fn web_task(
  task_id: usize,
  stack: embassy_net::Stack<'static>,
  app: &'static AppRouter<AppProps>,
  config: &'static Config<Duration>,
) -> ! {
  let mut tcp_rx_buffer = [0; 1024];
  let mut tcp_tx_buffer = [0; 1024];
  let mut http_buffer = [0; 2048];

  loop {
    if stack.is_link_up() {
      break;
    }
    Timer::after(Duration::from_millis(500)).await;
  }

  loop {
    if let Some(config) = stack.config_v4() {
      println!("server task #{task_id} at {}", config.address);
      break;
    }
    Timer::after(Duration::from_millis(500)).await;
  }

  loop {
    Server::new(app, config, &mut http_buffer)
      .listen_and_serve(task_id, stack, 80, &mut tcp_rx_buffer, &mut tcp_tx_buffer)
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

  let config = embassy_net::Config::dhcpv4(Default::default());
  let rng = Rng::new();
  let seed = (rng.random() as u64) << 32 | rng.random() as u64;

  let (stack, runner) = embassy_net::new(
    interfaces.sta,
    config,
    mk_static!(StackResources<3>, StackResources::<3>::new()),
    seed,
  );

  spawner.spawn(net_task(runner)).ok();
  spawner.spawn(connection(controller)).ok();
  spawner.spawn(buzzer_task(buzzer)).ok();
  spawner.spawn(watchdog_task()).ok();

  let app = make_static!(AppRouter<AppProps>, AppProps.build_app());
  let picoserve_config = make_static!(
    picoserve::Config::<Duration>,
    picoserve::Config::new(picoserve::Timeouts {
      start_read_request: Some(Duration::from_secs(5)),
      persistent_start_read_request: Some(Duration::from_secs(2)),
      read_request: Some(Duration::from_secs(2)),
      write: Some(Duration::from_secs(2)),
    })
    .keep_connection_alive()
  );

  for task_id in 0..2 {
    spawner.spawn(web_task(task_id, stack, app, picoserve_config)).ok();
  }

  loop {
    Timer::after(Duration::from_secs(1)).await;
  }
}