#![no_std]
#![no_main]
#![feature(impl_trait_in_assoc_type)]

extern crate alloc;
use alloc::boxed::Box;

use core::future;

use embassy_executor::Spawner;
use embassy_net::{Runner, StackResources};
use embassy_sync::{blocking_mutex::raw::CriticalSectionRawMutex, watch::Watch};
use embassy_time::{with_timeout, Duration, Instant, Timer};

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
  free_heap: u32,
}

#[derive(Clone, Copy)]
enum BuzzerState {
  Off,
  On(Instant),
}

static BUZZER_STATE: Watch<CriticalSectionRawMutex, BuzzerState, 2> =
  Watch::new_with(BuzzerState::Off);

struct ActivityMonitor {
  last_activity: Instant,
  timeout: Duration,
}

impl ActivityMonitor {
  fn new(timeout_secs: u64) -> Self {
    Self {
      last_activity: Instant::now(),
      timeout: Duration::from_secs(timeout_secs),
    }
  }

  fn mark_activity(&mut self) {
    self.last_activity = Instant::now();
  }

  fn is_timed_out(&self) -> bool {
    Instant::now().duration_since(self.last_activity) > self.timeout
  }
}

struct RateLimiter {
  last_event: Instant,
  min_interval: Duration,
}

impl RateLimiter {
  fn new(min_interval_ms: u64) -> Self {
    Self {
      last_event: Instant::now(),
      min_interval: Duration::from_millis(min_interval_ms),
    }
  }

  fn should_allow(&mut self) -> bool {
    let now = Instant::now();
    if now.duration_since(self.last_event) >= self.min_interval {
      self.last_event = now;
      true
    } else {
      false
    }
  }
}

struct ConnectionStats {
  msg_count: u32,
  connect_time: Instant,
  last_log: Instant,
  log_interval: Duration,
}

impl ConnectionStats {
  fn new(log_interval_secs: u64) -> Self {
    let now = Instant::now();
    Self {
      msg_count: 0,
      connect_time: now,
      last_log: now,
      log_interval: Duration::from_secs(log_interval_secs),
    }
  }

  fn increment(&mut self) {
    self.msg_count += 1;
  }

  fn should_log(&mut self) -> bool {
    let now = Instant::now();
    if now.duration_since(self.last_log) >= self.log_interval {
      self.last_log = now;
      true
    } else {
      false
    }
  }

  fn uptime_secs(&self) -> u64 {
    Instant::now().duration_since(self.connect_time).as_secs()
  }

  fn log_stats(&self) {
    println!("[WS] {} pings, uptime {}s", self.msg_count, self.uptime_secs());
  }

  fn log_disconnect(&self) {
    println!("[WS] disconnected - {} pings, {}s", self.msg_count, self.uptime_secs());
  }
}

struct BuzzerWebSocket;

impl BuzzerWebSocket {
  async fn handle_ping<W: embedded_io_async::Write>(
    data: &[u8],
    tx: &mut SocketTx<W>,
    stats: &mut ConnectionStats,
    rate_limiter: &mut RateLimiter,
    activity: &mut ActivityMonitor,
  ) -> Result<(), W::Error> {
    activity.mark_activity();

    if !rate_limiter.should_allow() {
      return Ok(());
    }

    stats.increment();

    if stats.should_log() {
      stats.log_stats();
    }

    BUZZER_STATE.sender().send(BuzzerState::On(Instant::now()));

    if let Ok(Err(e)) = with_timeout(Duration::from_millis(50), tx.send_pong(data)).await {
      println!("[WS] pong failed - {e:?}");
      return Err(e);
    }

    Ok(())
  }

  fn check_memory() -> Option<(u16, &'static str)> {
    let free_heap = esp_alloc::HEAP.free();
    if free_heap < 10_000 {
      println!("[WS] closing - low heap ({} bytes)", free_heap);
      Some((1011, "server overload"))
    } else {
      None
    }
  }
}

impl WebSocketCallback for BuzzerWebSocket {
  async fn run<R: embedded_io_async::Read, W: embedded_io_async::Write<Error = R::Error>>(
    self,
    mut rx: SocketRx<R>,
    mut tx: SocketTx<W>,
  ) -> Result<(), W::Error> {
    println!("[WS] connected");

    let mut buffer = Box::new([0u8; 512]);
    let mut stats = ConnectionStats::new(10);
    let mut rate_limiter = RateLimiter::new(10);
    let mut activity = ActivityMonitor::new(30);

    let close_reason = loop {
      if activity.is_timed_out() {
        println!("[WS] timeout - idle >30s");
        break Some((1000, "idle timeout"));
      }

      if let Some(reason) = Self::check_memory() {
        break Some(reason);
      }

      match rx.next_message(&mut *buffer, future::pending()).await?.ignore_never_b() {
        Ok(Message::Ping(data)) => {
          if let Err(_) = Self::handle_ping(data, &mut tx, &mut stats, &mut rate_limiter, &mut activity).await {
            break Some((1011, "send failed"));
          }
        }
        Ok(Message::Close(reason)) => {
          println!("[WS] close from client - {reason:?}");
          break None;
        }
        Ok(Message::Pong(_)) => {
          activity.mark_activity();
        }
        Err(error) => {
          println!("[WS] protocol error - {error:?}");
          break Some((error.code(), "protocol error"));
        }
        _ => {}
      }
    };

    BUZZER_STATE.sender().send(BuzzerState::Off);
    stats.log_disconnect();

    tx.close(close_reason).await
  }
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
            free_heap: esp_alloc::HEAP.free() as u32,
          })
        }),
      )
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
    Timer::after(Duration::from_secs(30)).await;

    let free_heap = esp_alloc::HEAP.free();
    println!("[WATCHDOG] heap: {}kb", free_heap / 1024);

    if free_heap < 20_000 {
      println!("[WATCHDOG] WARN - heap pressure");
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
      Err(_) => Timer::after(Duration::from_millis(5000)).await,
    }
  }
}

const WEB_TASK_COUNT: usize = 2;

#[embassy_executor::task(pool_size = WEB_TASK_COUNT)]
async fn web_task(
  task_id: usize,
  stack: embassy_net::Stack<'static>,
  app: &'static AppRouter<AppProps>,
  config: &'static Config<Duration>,
) -> ! {
  let mut tcp_rx_buffer = Box::new([0u8; 512]);
  let mut tcp_tx_buffer = Box::new([0u8; 512]);
  let mut http_buffer = Box::new([0u8; 1024]);

  loop {
    if stack.is_link_up() {
      break;
    }
    Timer::after(Duration::from_millis(500)).await;
  }

  loop {
    if let Some(config) = stack.config_v4() {
      println!("[SERVER] task #{task_id} at {}", config.address);
      break;
    }
    Timer::after(Duration::from_millis(500)).await;
  }

  loop {
    Server::new(app, config, &mut *http_buffer)
      .listen_and_serve(task_id, stack, 80, &mut *tcp_rx_buffer, &mut *tcp_tx_buffer)
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
      write: Some(Duration::from_secs(5)),
    })
    .keep_connection_alive()
  );

  for task_id in 0..WEB_TASK_COUNT {
    spawner.spawn(web_task(task_id, stack, app, picoserve_config)).ok();
  }

  loop {
    Timer::after(Duration::from_secs(1)).await;
  }
}