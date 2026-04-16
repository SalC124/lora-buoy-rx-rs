use rfm9x_rs::Rfm9x;

use core::convert::TryInto;

use embedded_svc::{
    http::client::Client as HttpClient,
    io::Write,
    utils::io,
    wifi::{AuthMethod, ClientConfiguration, Configuration},
};
use esp_idf_svc::hal::{
    delay::FreeRtos,
    gpio::PinDriver,
    peripherals::Peripherals,
    spi::{config::Config, Dma, SpiDeviceDriver, SpiDriver, SpiDriverConfig},
    units::FromValueType,
};
use esp_idf_svc::{
    eventloop::EspSystemEventLoop,
    http::client::{Configuration as HttpConfiguration, EspHttpConnection},
    nvs::EspDefaultNvsPartition,
    wifi::{BlockingWifi, EspWifi},
};
use log::*;

const SSID: &'static str = env!("SSID"); // of the wifi to connect to
const PASSWORD: &'static str = env!("PASS");

const URI: &'static str = concat!(env!("URL"), ":", env!("PORT")); // of the backend

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

    let config = &HttpConfiguration {
        crt_bundle_attach: Some(esp_idf_svc::sys::esp_crt_bundle_attach),
        ..Default::default()
    };

    let mut client = HttpClient::wrap(EspHttpConnection::new(&config)?);

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

    let mut rst_pin = PinDriver::output(peripherals.pins.gpio14).unwrap();
    rst_pin.set_low().unwrap();
    FreeRtos::delay_ms(10);
    rst_pin.set_high().unwrap();
    FreeRtos::delay_ms(10);

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

    let mut delay = FreeRtos;
    let mut radio = match Rfm9x::new_with_delay(spi_device, rst_pin, FREQUENCY, &mut delay) {
        Ok(r) => r,
        Err(rfm9x_rs::Error::InvalidVersion(ver)) => {
            panic!("Read 0x{:02X}, expected 0x12", ver);
        }
        Err(e) => panic!("Radio error: {:?}", e),
    };

    const FREQUENCY: u32 = 915;

    let mut buf = [0u8; 256];

    '_main_loop: loop {
        if let Ok(_len) = radio.poll_recv(&mut buf) {
            let payload = &buf;
            // info!("Received packet: {:?}", payload);

            if let Some(start) = payload.iter().position(|&b| b == b'{') {
                if let Some(end) = payload.iter().rposition(|&b| b == b'}') {
                    let json_slice = &payload[start..=end];
                    match post_request(&mut client, json_slice) {
                        Ok(r) => {
                            info!("[RESPONSE] {}", r)
                        }
                        Err(e) => {
                            error!("[ERROR]    {}", e)
                        }
                    };
                }
            }
        }
        FreeRtos::delay_ms(1000);
    }
}

fn post_request(
    client: &mut HttpClient<EspHttpConnection>,
    payload: &[u8],
) -> anyhow::Result<String> {
    // Prepare headers and URL
    let content_length_header = format!("{}", payload.len());
    let headers = [
        ("content-type", "text/plain"),
        ("content-length", &*content_length_header),
    ];

    // Send request
    let mut request = client.post(URI, &headers)?;
    request.write_all(payload)?;
    request.flush()?;
    info!("-> POST {URI}");
    let mut response = request.submit()?;

    // Process response
    let status = response.status();
    info!("<- {status}");
    let mut buf = [0u8; 1024];
    let bytes_read = io::try_read_full(&mut response, &mut buf).map_err(|e| e.0)?;
    info!("Read {bytes_read} bytes");
    let ret = match std::str::from_utf8(&buf[0..bytes_read]) {
        Ok(body_string) => Ok(format!(
            "Response body (truncated to {} bytes): {body_string:?}",
            buf.len()
        )),
        Err(e) => Err(anyhow::anyhow!(format!(
            "Error decoding response body: {e}"
        ))),
    };
    return ret;
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

    wifi.connect()?;
    info!("Wifi connected");

    wifi.wait_netif_up()?;
    info!("Wifi netif up");

    Ok(())
}
