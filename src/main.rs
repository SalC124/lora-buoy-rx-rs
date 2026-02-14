use core::convert::TryInto;
use embedded_svc::{
    http::{Method},
    io::{Write},
    wifi::{AuthMethod, ClientConfiguration, Configuration},
};
use esp_idf_svc::hal::{delay::FreeRtos, peripherals::Peripherals};
use esp_idf_svc::{
    eventloop::EspSystemEventLoop,
    http::server::EspHttpServer,
    nvs::EspDefaultNvsPartition,
    wifi::{BlockingWifi, EspWifi},
};
use log::*;
use serde::{Deserialize, Serialize};
use std::sync::Mutex;
use anyhow::Context;

const SSID: &'static str = env!("SSID"); // of the wifi to connect to
const PASSWORD: &'static str = env!("PASS");

static INDEX_HTML: &str = include_str!("http_server_page.html");

// Max payload length


// Need lots of stack to parse JSON
const STACK_SIZE: usize = 10240;

#[derive(Deserialize, Serialize)]
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

    connect_wifi(&mut wifi)?;

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

    LORA_DATA.lock().unwrap().packet = Some(0);
    // set up lora + spi
    loop {
        // lora.recv();

        {
            let mut data = LORA_DATA.lock().unwrap();
            if let Some(packets) = data.packet {
                data.packet = Some(packets + 1);
            }
        }
        FreeRtos::delay_ms(2000);
    }
}

fn connect_wifi(wifi: &mut BlockingWifi<EspWifi<'static>>) -> anyhow::Result<()> {
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

    Ok(())
}

fn create_server() -> anyhow::Result<EspHttpServer<'static>> {
    let server_configuration = esp_idf_svc::http::server::Configuration {
        stack_size: STACK_SIZE,
        ..Default::default()
    };

    Ok(EspHttpServer::new(&server_configuration)?)
}
