#![warn(clippy::all)]

mod mqtt_discovery;
mod settings;
use crate::mqtt_discovery::run_mqtt_discovery;
use crate::settings::MqttSettings;
use settings::Settings;

use masterpower_api::commands::qid::QID;
use masterpower_api::commands::qmod::QMOD;
use masterpower_api::commands::qpgs::{QPGS0, QPGS1, QPGS2, QPGS3, QPGS4, QPGS5, QPGS6, QPGS7, QPGS8, QPGS9};
use masterpower_api::commands::qpi::QPI;
use masterpower_api::commands::qpigs::QPIGS;
use masterpower_api::commands::qpiri::QPIRIReduced;
use masterpower_api::commands::qpiri::QPIRI;
use masterpower_api::commands::qpiws::QPIWS;
use masterpower_api::commands::qvfw::QVFW;
// use masterpower_api::commands::qvfw2::QVFW2;
// use masterpower_api::commands::qvfw3::QVFW3;
use masterpower_api::inverter::Inverter;

use libc::{open, O_RDWR};
use log::{debug, error, info};
use mqtt_async_client::client::{Client as MQTTClient, KeepAlive, Publish as PublishOpts, QoS};
use serde_derive::Serialize;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::io::FromRawFd;
use std::path::Path;
use std::thread::sleep;
use std::time::Instant;
use tokio::fs::File;
use tokio::time::Duration;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("Starting {} version {}", env!("CARGO_PKG_NAME").to_ascii_uppercase(), env!("CARGO_PKG_VERSION"));

    // Load configuration
    let settings = match Settings::new() {
        Ok(settings) => settings,
        Err(e) => {
            println!("Error loading configuration file: {}", e);
            std::process::exit(1);
        }
    };

    // Enable debugging
    if settings.debug {
        std::env::set_var("RUST_LOG", "warn,mpqtt=trace,masterpower_api=trace");
        info!("Enabled debug output");
    } else {
        std::env::set_var("RUST_LOG", "error,mpqtt=info,masterpower_api=info");
    }
    pretty_env_logger::init_timed();

    // Create MQTT Connection
    info!("Connecting to MQTT Broker at: {}:{}", settings.mqtt.host, settings.mqtt.port);
    let mut builder = mqtt_async_client::client::Client::builder();
    let mut mqtt_client = match builder
        .set_host(settings.mqtt.host.clone())
        .set_port(settings.mqtt.port)
        .set_username(Option::from(settings.mqtt.username.clone()))
        .set_password(Option::from(settings.mqtt.password.as_bytes().to_vec()))
        .set_client_id(Option::from(settings.mqtt.client_id.clone()))
        .set_connect_retry_delay(Duration::from_secs(1))
        .set_keep_alive(KeepAlive::from_secs(5))
        .set_operation_timeout(Duration::from_secs(10))
        .set_automatic_connect(true)
        .build()
    {
        Ok(val) => val,
        Err(err) => {
            error!("Problem with MQTT client builder: {}", err);
            std::process::exit(0);
        }
    };

    mqtt_client.connect().await?;
    info!("Connected to MQTT Broker");

    // Run MQTT Discovery
    run_mqtt_discovery(&mqtt_client, &settings.mqtt, settings.inverter_count, &settings.mode).await?;

    // Open inverter tty device -
    // TODO wrap open call in for loop with timeout and a break on success
    let stream = match raw_open(settings.inverter.path.clone()) {
        Ok(stream) => stream,
        Err(err) => {
            // Handle error opening inverter
            // TODO wrap in loop to retry publish on fails
            publish_error(&mqtt_client, &settings.mqtt, err.to_string()).await?;
            error!("Could not open inverter communication {}", err);
            todo!("implement retrying on file not found or couldn't open with warn! before error!");
        }
    };

    // Clear previous errors
    // TODO wrap in loop to retry publish on fails
    clear_error(&mqtt_client, &settings.mqtt).await?;

    // Create inverter instance
    let mut inverter = Inverter::from_stream(stream);

    // Start
    let init_res = init(&mut inverter, &mqtt_client, &settings).await;
    if let Err(error) = init_res {
        publish_error(&mqtt_client, &settings.mqtt, error.to_string()).await?;
        error!("Error initialising inverter: {}", error);
        todo!("implement retrying on file not found or couldn't open with warn! before error!");
        // std::process::exit(1);
    }

    // Update loop
    loop {
        match update(&mut inverter, &mqtt_client, &settings).await {
            Err(error) => {
                publish_error(&mqtt_client, &settings.mqtt, error.to_string()).await?;
                error!("Published error: {} - sleeping for {}", error, settings.error_delay);
                // hopefully this can help it sort itself out on errors
                // before going straight back into the next update
                sleep(Duration::from_secs(settings.error_delay));
            }
            Ok(()) => match clear_error(&mqtt_client, &settings.mqtt).await {
                Ok(()) => (),
                Err(error) => {
                    error!("Failed to clear error: {}", error)
                }
            },
        }
    }
}

async fn init(inverter: &mut Inverter<File>, mqtt_client: &MQTTClient, settings: &Settings) -> Result<(), Box<dyn std::error::Error>> {
    // Get initial values

    // QID      - Serial number
    match inverter.execute::<QID>(()).await {
        Ok(serial_number) => {
            publish_update(&mqtt_client, &settings.mqtt, "qid", serde_json::to_string(&serial_number)?).await?;
        }
        Err(serial_number_error) => {
            error!("Error fetching serial number: {}", serial_number_error);
        }
    };
    // QPI      - Protocol ID
    let protocol_id = inverter.execute::<QPI>(()).await?;
    publish_update(&mqtt_client, &settings.mqtt, "qpi", serde_json::to_string(&protocol_id)?).await?;

    // QVFW     - Software version 1
    let software_version_1 = inverter.execute::<QVFW>(()).await?;
    publish_update(&mqtt_client, &settings.mqtt, "qvfw", serde_json::to_string(&software_version_1)?).await?;

    debug!("Completed init commands");

    Ok(())
}

async fn update(inverter: &mut Inverter<File>, mqtt_client: &MQTTClient, settings: &Settings) -> Result<(), Box<dyn std::error::Error>> {
    // Start update
    debug!("Starting new update");
    let outer_start = Instant::now();
    // QPGSn    - Device general status parameters inquiry
    for _ in 0..settings.inner_iterations {
        let inner_start = Instant::now();
        if settings.mode == String::from("phocos") {
            let start_index = if settings.debug { 0 } else { 1 };
            for index in start_index..=settings.inverter_count {
                let qpgs = match index {
                    0 => inverter.execute::<QPGS0>(()).await?,
                    1 => inverter.execute::<QPGS1>(()).await?,
                    2 => inverter.execute::<QPGS2>(()).await?,
                    3 => inverter.execute::<QPGS3>(()).await?,
                    4 => inverter.execute::<QPGS4>(()).await?,
                    5 => inverter.execute::<QPGS5>(()).await?,
                    6 => inverter.execute::<QPGS6>(()).await?,
                    7 => inverter.execute::<QPGS7>(()).await?,
                    8 => inverter.execute::<QPGS8>(()).await?,
                    9 => inverter.execute::<QPGS9>(()).await?,
                    _ => unimplemented!(),
                };
                if (settings.debug && index == 0) || index != 0 {
                    publish_update(&mqtt_client, &settings.mqtt, &format!("qpgs{}", index), serde_json::to_string(&qpgs)?).await?;
                }
            }
        }

        // QPIGS    - Device general status parameters inquiry
        if settings.mode != String::from("phocos") {
            let qpigs = inverter.execute::<QPIGS>(()).await?;
            publish_update(&mqtt_client, &settings.mqtt, "qpigs", serde_json::to_string(&qpigs)?).await?;
        }

        // inner loop reporting
        let inner_time = inner_start.elapsed().as_millis();
        info!("Partial update took {}ms - sleeping for {}s", inner_time, settings.inner_delay);
        // inner_loop_duration can essentially be our heartbeat
        let inner_stats = Stats { update_duration: inner_time };
        publish_update(&mqtt_client, &settings.mqtt, "inner_stats", serde_json::to_string(&inner_stats)?).await?;
        sleep(Duration::from_secs(settings.inner_delay));
    }

    // QMOD     -  Device Mode Inquiry
    let qmod = inverter.execute::<QMOD>(()).await?;
    publish_update(&mqtt_client, &settings.mqtt, "qmod", serde_json::to_string(&qmod)?).await?;

    // QPIWS    - Device Warning Status Inquiry
    let qpiws = inverter.execute::<QPIWS>(()).await?;
    publish_update(&mqtt_client, &settings.mqtt, "qpiws", serde_json::to_string(&qpiws)?).await?;

    // QPIRI    - Device Rating Information Inquiry
    if settings.mode != String::from("phocos") {
        let qpiri = inverter.execute::<QPIRI>(()).await?;
        publish_update(&mqtt_client, &settings.mqtt, "qpiri", serde_json::to_string(&qpiri)?).await?;
    } else {
        let qpiri = inverter.execute::<QPIRIReduced>(()).await?;
        publish_update(&mqtt_client, &settings.mqtt, "qpiri", serde_json::to_string(&qpiri)?).await?;
    }

    // Report update completed
    let outer_time = outer_start.elapsed().as_millis();
    info!("Full update took {}ms - sleeping for {}s", outer_time, settings.outer_delay);
    let outer_stats = Stats { update_duration: outer_time };
    publish_update(&mqtt_client, &settings.mqtt, "outer_stats", serde_json::to_string(&outer_stats)?).await?;
    sleep(Duration::from_secs(settings.outer_delay));
    Ok(())
}

async fn publish_update(mqtt_client: &MQTTClient, mqtt: &MqttSettings, command: &str, value: String) -> Result<(), Box<dyn std::error::Error>> {
    let mut msg = PublishOpts::new(format!("{}/{}", mqtt.topic, command).to_string(), Vec::from(value));
    msg.set_qos(QoS::AtLeastOnce);
    msg.set_retain(false);
    for _ in 0..5 {
        match mqtt_client.publish(&msg).await {
            Ok(()) => break,
            Err(pub_error) => error!("Error publishing update for {}: {}", command, pub_error),
        };
    }
    Ok(())
}

async fn publish_error(mqtt_client: &MQTTClient, mqtt: &MqttSettings, error: String) -> Result<(), Box<dyn std::error::Error>> {
    let mut msg = PublishOpts::new(format!("{}/error", mqtt.topic).to_string(), Vec::from(error.clone()));
    msg.set_qos(QoS::AtLeastOnce);
    msg.set_retain(false);
    for _ in 0..5 {
        match mqtt_client.publish(&msg).await {
            Ok(()) => break,
            Err(pub_error) => error!("Error publishing error: {} - {}", pub_error, error),
        };
    }
    Ok(())
}

async fn clear_error(mqtt_client: &MQTTClient, mqtt: &MqttSettings) -> Result<(), Box<dyn std::error::Error>> {
    let mut msg = PublishOpts::new(format!("{}/error", mqtt.topic).to_string(), "".to_string().as_bytes().to_vec());
    msg.set_qos(QoS::AtLeastOnce);
    msg.set_retain(false);
    for _ in 0..5 {
        match mqtt_client.publish(&msg).await {
            Ok(()) => break,
            Err(pub_error) => error!("Error clearing error: {}", pub_error),
        };
    }
    Ok(())
}

fn raw_open<P: AsRef<Path>>(path: P) -> std::io::Result<File> {
    let fd = unsafe { open(path.as_ref().as_os_str().as_bytes().as_ptr() as *const u8, O_RDWR) };
    if fd < 0 {
        return Err(std::io::Error::last_os_error());
    }

    let std_file = unsafe { std::fs::File::from_raw_fd(fd) };
    Ok(File::from_std(std_file))
}

#[derive(Serialize, Debug)]
struct Stats {
    update_duration: u128,
}
