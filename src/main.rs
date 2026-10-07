use std::error::Error;
use std::process::Command;
use std::time::Duration;
use btleplug::api::{bleuuid, Central, Manager as _, Peripheral as _, ScanFilter, WriteType};
use btleplug::platform::{Adapter, Manager, Peripheral};
use bytes::{Buf, BytesMut};
use futures::StreamExt;
use tokio::time;
use uuid::Uuid;

const UUID_RX: Uuid = bleuuid::uuid_from_u16(0xfff2);
const UUID_TX: Uuid = bleuuid::uuid_from_u16(0xfff1);
const FULL_READ_REQ: [u8; 8] = [0x81, 0x03, 0x00, 0x38, 0x00, 0x03, 0x9B, 0xC6];

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let runtime_dir = std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| "/tmp".into());
    let manager = Manager::new().await?;
    let adapters = manager.adapters().await?;
    let central = adapters
        .into_iter()
        .next()
        .ok_or("Bluetooth-адаптер не найден")?;

    loop {
        if let Err(e) = run_bms_session(&central, &runtime_dir).await {
            eprintln!("Ошибка сессии BMS: {e}. Переподключение через 5 сек...");
            write_waybar_offline(&runtime_dir);
            time::sleep(Duration::from_secs(5)).await;
        }
    }
}

async fn run_bms_session(central: &Adapter, runtime_dir: &str) -> Result<(), Box<dyn Error>> {
    central.start_scan(ScanFilter::default()).await?;
    time::sleep(Duration::from_secs(4)).await;

    let bms = find_bms(central).await.ok_or("BMS не найдена поблизости")?;
    bms.connect().await?;
    bms.discover_services().await?;

    let chars = bms.characteristics();
    let rx_char = chars.iter().find(|c| c.uuid == UUID_RX).ok_or("RX не найден")?.clone();
    let tx_char = chars.iter().find(|c| c.uuid == UUID_TX).ok_or("TX не найден")?.clone();

    bms.subscribe(&tx_char).await?;
    let mut notifications = bms.notifications().await?;

    println!("Подключено к BMS. Трансляция телеметрии в Waybar... {}", bms.properties().await.unwrap().unwrap().local_name.unwrap());

    let mut buf = BytesMut::with_capacity(512);

    loop {
        bms.write(&rx_char, &FULL_READ_REQ, WriteType::WithoutResponse).await?;
        buf.clear();

        let read_result = time::timeout(Duration::from_secs(3), async {
            while buf.len() < 11 {
                if let Some(packet) = notifications.next().await {
                    println!("Got {} {:?}", packet.uuid, packet.value);
                    if packet.uuid == UUID_TX {
                        buf.extend_from_slice(&packet.value);

                        if buf.len() == 5 && (buf[1] & 0x80 != 0) {
                            eprintln!(
                                "Modbus Exception: код 0x{:02X} (байт ошибки: 0x{:02X})",
                                buf[1], buf[2]
                            );
                            break;
                        }
                    }
                } else {
                    return Err("Поток уведомлений закрыт");
                }
            }
            Ok(())
        })
            .await;

        match read_result {
            Ok(Ok(_)) => {
                if buf.len() >= 11 && (buf[0] == 0x51 || buf[0] == 0x81) && buf[1] == 0x03 && buf[2] == 0x06 {
                    let mut data = buf.split_to(11);
                    data.advance(3);

                    let volt = f32::from(data.get_u16()) * 0.1;
                    let current = (i32::from(data.get_u16()) - 30000) as f32 * 0.1;
                    let soc = (f32::from(data.get_u16()) * 0.1).clamp(0.0, 100.0);

                    update_waybar_json(runtime_dir, soc, volt, current);
                } else if buf.len() != 5 {
                    eprintln!("Неизвестный ответ: {:02X?}", &buf[..]);
                }
            }
            _ => {
                eprintln!("Таймаут ответа BMS");
                if !bms.is_connected().await.unwrap_or(false) {
                    return Err("Соединение с BMS разорвано".into());
                }
            }
        }

        time::sleep(Duration::from_secs(5)).await;
    }
}

fn update_waybar_json(runtime_dir: &str, soc: f32, volt: f32, current: f32) {
    let power = volt * current.abs();

    let (status_icon, class) = if current > 0.1 {
        ("󰂄", "charging")
    } else if soc <= 20.0 {
        ("󰂃", "critical")
    } else if current < -0.1 {
        ("󰁹", "discharging")
    } else {
        ("󰚥", "idle")
    };

    let text = format!("{status_icon} {soc:.0}% ({current:+.1}A)");
    let tooltip = format!(
        "BMS Аккумулятор\\n\
         Заряд: {soc:.1}%\\n\
         Напряжение: {volt:.2} В\\n\
         Ток: {current:+.2} А\\n\
         Мощность: {power:.1} Вт"
    );

    let json = format!(
        r#"{{"text":"{text}","tooltip":"{tooltip}","class":"{class}","percentage":{percentage}}}"#,
        percentage = soc.round() as u32
    );

    let target = format!("{runtime_dir}/bms.json");
    let tmp = format!("{runtime_dir}/bms.json.tmp");
    if std::fs::write(&tmp, &json).is_ok() {
        let _ = std::fs::rename(&tmp, &target);
        let _ = Command::new("pkill").args(["-RTMIN+9", "waybar"]).output();
    }
}

fn write_waybar_offline(runtime_dir: &str) {
    let json = r#"{"text":"󰂲 Offline","tooltip":"BMS не подключена","class":"offline","percentage":0}"#;
    let target = format!("{runtime_dir}/bms.json");
    let _ = std::fs::write(target, json);
    let _ = Command::new("pkill").args(["-RTMIN+9", "waybar"]).output();
}

async fn find_bms(central: &Adapter) -> Option<Peripheral> {
    let peripherals = central.peripherals().await.ok()?;
    for p in peripherals {
        if let Ok(Some(props)) = p.properties().await {
            if let Some(name) = props.local_name {
                if name.starts_with("DL-") || name.starts_with("DLBT") || name.contains("BMS") {
                    return Some(p);
                }
            }
        }
    }
    None
}
