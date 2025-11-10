#![no_std]
#![no_main]

use embedded_io_async::Write;
use embassy_executor::Spawner;
use embassy_net::{Runner, StackResources, tcp::TcpSocket};
use embassy_time::{Duration, Timer};

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

#[esp_rtos::main]
async fn main(spawner: Spawner) -> ! {
  esp_println::logger::init_logger_from_env();
  let config = esp_hal::Config::default().with_cpu_clock(CpuClock::max());
  let peripherals = esp_hal::init(config);

  esp_alloc::heap_allocator!(#[ram(reclaimed)] size: 64 * 1024);
  esp_alloc::heap_allocator!(size: 36 * 1024);

  let buzzer = Output::new(peripherals.GPIO5, Level::Low, OutputConfig::default());
  let buzzer = mk_static!(Output<'static>, buzzer);

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

  let stack = mk_static!(embassy_net::Stack<'static>, stack);

  spawner.spawn(connection(controller)).ok();
  spawner.spawn(net_task(runner)).ok();
  spawner.spawn(buzzer_server(stack, buzzer)).ok();  // Add & here

  loop {
    Timer::after(Duration::from_secs(1)).await;
  }
}

#[embassy_executor::task]
async fn buzzer_server(
  stack: &'static embassy_net::Stack<'static>,
  buzzer: &'static mut Output<'static>,
) {
  loop {
    if stack.is_link_up() {
      break;
    }
    Timer::after(Duration::from_millis(500)).await;
  }

  loop {
    if let Some(config) = stack.config_v4() {
      println!("Buzzer server ready at: {}", config.address);
      break;
    }
    Timer::after(Duration::from_millis(500)).await;
  }

  loop {
    let mut rx_buffer = [0; 1024];
    let mut tx_buffer = [0; 1024];
    let mut socket = TcpSocket::new(*stack, &mut rx_buffer, &mut tx_buffer);  // Add * here
    socket.set_timeout(Some(Duration::from_secs(30)));

    println!("Listening on port 80...");
    if let Err(e) = socket.accept(80).await {
      println!("Accept error: {:?}", e);
      Timer::after(Duration::from_secs(1)).await;
      continue;
    }

    println!("Client connected!");

    let mut buf = [0u8; 512];
    let mut total_read = 0;

    loop {
      match socket.read(&mut buf[total_read..]).await {
        Ok(0) => break,
        Ok(n) => {
          total_read += n;
          if total_read >= 4 && &buf[total_read-4..total_read] == b"\r\n\r\n" {
            break;
          }
          if total_read >= buf.len() {
            break;
          }
        }
        Err(e) => {
          println!("Read error: {:?}", e);
          break;
        }
      }
    }

    let request = core::str::from_utf8(&buf[..total_read]).unwrap_or("");
    println!("Request: {}", &request[..request.len().min(100)]);

    if request.starts_with("POST /buzz/start") {
      buzzer.set_high();
      println!("Buzzer ON");
      let response = b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\r\n{\"status\":\"buzzing\"}";
      let _ = socket.write_all(response).await;
    } else if request.starts_with("POST /buzz/stop") {
      buzzer.set_low();
      println!("Buzzer OFF");
      let response = b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\r\n{\"status\":\"stopped\"}";
      let _ = socket.write_all(response).await;
    } else if request.starts_with("POST /buzz/heartbeat") {
      let response = b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\r\n{\"status\":\"alive\"}";
      let _ = socket.write_all(response).await;
    } else if request.starts_with("GET /status") {
      let response = b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\r\n{\"status\":\"ok\"}";
      let _ = socket.write_all(response).await;
    } else {
      let response = b"HTTP/1.1 404 Not Found\r\n\r\n";
      let _ = socket.write_all(response).await;
    }

    socket.close();
    Timer::after(Duration::from_millis(100)).await;
  }
}

#[embassy_executor::task]
async fn connection(mut controller: WifiController<'static>) {
  println!("Starting connection task");
  loop {
    match esp_radio::wifi::sta_state() {
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
      println!("Starting wifi");
      controller.start_async().await.unwrap();
      println!("Wifi started!");
    }
    println!("Connecting to wifi...");

    match controller.connect_async().await {
      Ok(_) => println!("Wifi connected!"),
      Err(e) => {
        println!("Failed to connect: {e:?}");
        Timer::after(Duration::from_millis(5000)).await
      }
    }
  }
}

#[embassy_executor::task]
async fn net_task(mut runner: Runner<'static, WifiDevice<'static>>) {
  runner.run().await
}