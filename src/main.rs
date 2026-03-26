use anyhow::Context;
use rfm9x_rs::Rfm9x;

use core::convert::TryInto;
use embedded_svc::{
    http::Method,
    io::Write,
    wifi::{AuthMethod, ClientConfiguration, Configuration},
};
use esp_idf_svc::hal::{
    delay::FreeRtos,
    gpio::PinDriver,
    peripherals::Peripherals,
    spi::{
        config::Config, Dma, Spi, SpiBusDriver, SpiConfig, SpiDeviceDriver, SpiDriver,
        SpiDriverConfig,
    },
    units::FromValueType,
};
use esp_idf_svc::{
    eventloop::EspSystemEventLoop,
    http::server::EspHttpServer,
    nvs::EspDefaultNvsPartition,
    wifi::{BlockingWifi, EspWifi},
};
use log::*;
use serde::{Deserialize, Serialize};
use std::sync::Mutex;

const SSID: &'static str = env!("SSID"); // of the wifi to connect to
const PASSWORD: &'static str = env!("PASS");

static INDEX_HTML: &str = include_str!("http_server_page.html");

// Need lots of stack to parse JSON
const STACK_SIZE: usize = 10240;

#[derive(Deserialize, Serialize, Debug)]
struct Data {
    temp: Option<f32>,
    kpa: Option<f32>,
    humi: Option<f32>,
    rssi: Option<f32>,
    packet: Option<i32>,
}

static LORA_DATA: Mutex<Data> = Mutex::new(Data {
    temp: None,
    kpa: None,
    humi: None,
    rssi: None,
    packet: None,
});

fn main() -> anyhow::Result<()> {
    esp_idf_svc::sys::link_patches();
    esp_idf_svc::log::EspLogger::initialize_default();

    // Setup Wifi
    let peripherals = Peripherals::take()?;
    let sys_loop = EspSystemEventLoop::take()?;
    let nvs = EspDefaultNvsPartition::take()?;

    let mut wifi = BlockingWifi::wrap(
        EspWifi::new(peripherals.modem, sys_loop.clone(), Some(nvs))?,
        sys_loop,
    )?;

    let url = connect_wifi(&mut wifi)?;

    let mut server = create_server()?;

    server.fn_handler("/", Method::Get, |req| {
        req.into_ok_response()?
            .write_all(INDEX_HTML.as_bytes())
            .map(|_| ())
    })?;
    server.fn_handler::<anyhow::Error, _>("/data", Method::Get, |req| {
        let data = LORA_DATA.lock().unwrap();
        let json = serde_json::to_string(&*data)?;

        req.into_ok_response()
            .context("Failed to create response")?
            .write_all(json.as_bytes())
            .context("Failed to write response")?;
        Ok(())
    })?;

    let spi = SpiDriver::new(
        peripherals.spi2,
        peripherals.pins.gpio18,       // sck
        peripherals.pins.gpio23,       // copi/mosi
        Some(peripherals.pins.gpio19), // cipo/miso
        &SpiDriverConfig::new().dma(Dma::Disabled),
    )
    .unwrap();

    let mut spi_device = SpiDeviceDriver::new(
        spi,
        Some(peripherals.pins.gpio5), // cs
        &Config::new().baudrate(1_u32.MHz().into()),
    )
    .unwrap();

    // Remove the old rst pin creation from before
    // let rst = PinDriver::output(peripherals.pins.gpio14).unwrap();

    info!("=== RFM9x SPI Diagnostic ===");

    // Manual reset sequence
    info!("Resetting RFM9x...");
    let mut rst_pin = PinDriver::output(peripherals.pins.gpio14).unwrap();
    rst_pin.set_low().unwrap();
    FreeRtos::delay_ms(10);
    rst_pin.set_high().unwrap();
    FreeRtos::delay_ms(10);

    // Test reads
    info!("Reading version register (0x42)...");
    for attempt in 0..5 {
        let mut read_buf = [0x42 & 0x7F, 0x00];
        if let Ok(()) = spi_device.transfer_in_place(&mut read_buf) {
            info!(
                "  Attempt {}: 0x{:02X} (expect 0x12)",
                attempt + 1,
                read_buf[1]
            );
        }
        FreeRtos::delay_ms(50);
    }

    // Initialize with better delays
    let mut delay = FreeRtos;
    let mut radio = match Rfm9x::new_with_delay(spi_device, rst_pin, FREQUENCY, &mut delay) {
        Ok(r) => {
            info!("✓ Radio initialized!");
            r
        }
        Err(rfm9x_rs::Error::InvalidVersion(ver)) => {
            error!("✗ Read 0x{:02X}, expected 0x12", ver);
            panic!("Check wiring and chip type!");
        }
        Err(e) => panic!("Radio error: {:?}", e),
    };

    const FREQUENCY: u32 = 915;

    // let mut radio = Rfm9x::new(spi_device, rst_pin, FREQUENCY).expect("Radio init failed");
    let mut buf = [0u8; 256];

    // set up lora + spi
    '_main_loop: loop {
        if let Ok(_len) = radio.poll_recv(&mut buf) {
            let payload = &buf;
            info!("Received packet: {:?}", payload);

            if let Some(start) = payload.iter().position(|&b| b == b'{') {
                if let Some(end) = payload.iter().rposition(|&b| b == b'}') {
                    let json_slice = &payload[start..=end];
                    if let Ok(payload_str) = core::str::from_utf8(json_slice) {
                        info!("Payload as string: {}", payload_str);
                        match serde_json::from_str::<Data>(payload_str.trim()) {
                            Ok(parsed) => {
                                let mut data = LORA_DATA.lock().unwrap();
                                *data = parsed;
                            }
                            Err(e) => error!("JSON parse error: {}", e),
                        }
                    }
                }
            }
        }
        FreeRtos::delay_ms(1000);
    }
}

fn connect_wifi(wifi: &mut BlockingWifi<EspWifi<'static>>) -> anyhow::Result<String> {
    let wifi_configuration: Configuration = Configuration::Client(ClientConfiguration {
        ssid: SSID.try_into().unwrap(),
        bssid: None,
        auth_method: AuthMethod::WPA2Personal,
        password: PASSWORD.try_into().unwrap(),
        channel: None,
        ..Default::default()
    });

    wifi.set_configuration(&wifi_configuration)?;
    wifi.start()?;
    info!("Wifi started");

    // Scan for networks
    info!("Scanning for WiFi networks...");
    let scan_result = wifi.scan()?;
    for ap in scan_result {
        info!(
            "Found AP: SSID='{}', Signal: {}, Auth: {:?}",
            ap.ssid, ap.signal_strength, ap.auth_method
        );
    }

    info!("Attempting to connect...");
    wifi.connect()?;
    info!("Wifi connected");

    wifi.wait_netif_up()?;
    info!("Wifi netif up");

    let ip_info = wifi.wifi().sta_netif().get_ip_info()?;
    info!("Wifi DHCP info: {:?}", ip_info);
    info!("Visit http://{}", ip_info.ip);

    let ip_address = format!("{}", ip_info.ip);

    let url: String = ip_address
        .split('.')
        .map(|octet| format!("{:02X}", octet.parse::<u8>().unwrap()))
        .collect::<Vec<String>>()
        .join(".");

    Ok(url)
}

fn create_server() -> anyhow::Result<EspHttpServer<'static>> {
    let server_configuration = esp_idf_svc::http::server::Configuration {
        stack_size: STACK_SIZE,
        ..Default::default()
    };

    Ok(EspHttpServer::new(&server_configuration)?)
}
