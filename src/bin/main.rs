#![no_std]
#![no_main]
#![feature(impl_trait_in_assoc_type)]

use embassy_executor::Spawner;
use embassy_net::{Runner, StackResources};
use embassy_sync::{blocking_mutex::raw::CriticalSectionRawMutex, mutex::Mutex};
use embassy_time::Duration;

use esp_alloc as _;
use esp_backtrace as _;

#[cfg(target_arch = "riscv32")]
use esp_hal::interrupt::software::SoftwareInterruptControl;
use esp_hal::{
  clock::CpuClock, 
  gpio::{Level, Output, OutputConfig}, 
  ram, 
  rng::Rng, 
  timer::timg::TimerGroup
};

use esp_println::println;

use esp_radio::{
  Controller,
  wifi::{
    ClientConfig, ModeConfig, WifiController, WifiDevice, WifiEvent, WifiStaState,
  },
};

use picoserve::{
  make_static,
  response::Json,
  routing::{get, post, PathRouter},
  AppBuilder, AppRouter,
};

esp_bootloader_esp_idf::esp_app_desc!();

macro_rules! mk_static {
  ($t:ty,$val:expr) => {{
    static STATIC_CELL: static_cell::StaticCell<$t> = static_cell::StaticCell::new();
    #[deny(unused_attributes)]
    let x = STATIC_CELL.uninit().write(($val));
    x
  }};
}

const SSID: &str = env!("SSID");
const PASSWORD: &str = env!("PASSWORD");

// Shared buzzer state
type BuzzerMutex = Mutex<CriticalSectionRawMutex, Output<'static>>;

#[derive(Clone, Copy)]
struct SharedBuzzer(&'static BuzzerMutex);

struct AppState {
  buzzer: SharedBuzzer,
}

impl picoserve::extract::FromRef<AppState> for SharedBuzzer {
  fn from_ref(state: &AppState) -> Self {
    state.buzzer
  }
}

// Response types
#[derive(serde::Serialize)]
struct StatusResponse {
  status: &'static str,
}

struct AppProps {
  state: AppState,
}

impl AppBuilder for AppProps {
  type PathRouter = impl PathRouter;

  fn build_app(self) -> picoserve::Router<Self::PathRouter> {
    use picoserve::extract::State;
    let Self { state } = self;

    picoserve::Router::new()
      .route(
        "/buzz/start",
        post(|State(SharedBuzzer(buzzer)): State<SharedBuzzer>| async move {
          println!("Buzzer ON");
          buzzer.lock().await.set_high();
          Json(StatusResponse { status: "buzzing" })
        }),
      )
      .route(
        "/buzz/stop",
        post(|State(SharedBuzzer(buzzer)): State<SharedBuzzer>| async move {
          println!("Buzzer OFF");
          buzzer.lock().await.set_low();
          Json(StatusResponse { status: "stopped" })
        }),
      )
      .route(
        "/buzz/heartbeat",
        post(|| async move {
          Json(StatusResponse { status: "alive" })
        }),
      )
      .route(
        "/status",
        get(|| async move {
          Json(StatusResponse { status: "ok" })
        }),
      )
      .with_state(state)
  }
}

const WEB_TASK_POOL_SIZE: usize = 1;

#[embassy_executor::task(pool_size = WEB_TASK_POOL_SIZE)]
async fn web_task(
  task_id: usize,
  stack: embassy_net::Stack<'static>,
  app: &'static AppRouter<AppProps>,
  config: &'static picoserve::Config<Duration>,
) -> ! {
  let port = 80;
  let mut tcp_rx_buffer = [0; 1024];
  let mut tcp_tx_buffer = [0; 1024];
  let mut http_buffer = [0; 2048];

  // Wait for network to be ready
  loop {
    if stack.is_link_up() {
      break;
    }
    embassy_time::Timer::after(Duration::from_millis(500)).await;
  }

  loop {
    if let Some(config) = stack.config_v4() {
      println!("Buzzer server ready at: {}", config.address);
      break;
    }
    embassy_time::Timer::after(Duration::from_millis(500)).await;
  }

  loop {
    picoserve::Server::new(app, config, &mut http_buffer)
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

  spawner.spawn(connection(controller)).ok();
  spawner.spawn(net_task(runner)).ok();

  // Create shared buzzer state wrapped in mutex
  let shared_buzzer = SharedBuzzer(
    mk_static!(BuzzerMutex, Mutex::new(buzzer))
  );

  // Build the picoserve app
  let app = make_static!(
    AppRouter<AppProps>,
    AppProps {
      state: AppState { buzzer: shared_buzzer }
    }
    .build_app()
  );

  // Configure picoserve with timeouts
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

  // Spawn single web server task
  spawner.spawn(web_task(0, stack, app, picoserve_config)).ok();

  loop {
    embassy_time::Timer::after(Duration::from_secs(1)).await;
  }
}

#[embassy_executor::task]
async fn connection(mut controller: WifiController<'static>) {
  println!("Starting connection task");
  loop {
    match esp_radio::wifi::sta_state() {
      WifiStaState::Connected => {
        controller.wait_for_event(WifiEvent::StaDisconnected).await;
        embassy_time::Timer::after(Duration::from_millis(5000)).await
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
      println!("Starting wifi");
      controller.start_async().await.unwrap();
      println!("Wifi started!");
    }
    println!("Connecting to wifi...");

    match controller.connect_async().await {
      Ok(_) => println!("Wifi connected!"),
      Err(e) => {
        println!("Failed to connect: {e:?}");
        embassy_time::Timer::after(Duration::from_millis(5000)).await
      }
    }
  }
}

#[embassy_executor::task]
async fn net_task(mut runner: Runner<'static, WifiDevice<'static>>) {
  runner.run().await
}