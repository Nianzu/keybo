#![no_std]
#![no_main]

use esp32_hid::{hid_config::HidConfig, keyboard::Keyboard, keycodes};
extern crate alloc;
use embassy_executor::Spawner;
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::{blocking_mutex::raw::NoopRawMutex, mutex::Mutex, signal::Signal};
use embassy_time::{Duration, Timer};
use esp_backtrace as _;
use esp_hal::efuse::Efuse;
use esp_hal::{
    analog::adc::{Adc, AdcConfig, Attenuation},
    clock::CpuClock,
    gpio::{Input, InputConfig, Pull},
    otg_fs::Usb,
    peripherals::{ADC1, TIMG1},
    timer::timg::{MwdtStage, MwdtStageAction, TimerGroup, Wdt},
};
use esp_hal::{rmt::Rmt, time::Rate};
use esp_hal_smartled::{SmartLedsAdapter, smart_led_buffer};
use esp_radio::Controller;
use esp_radio::esp_now::{
    BROADCAST_ADDRESS, EspNowManager, EspNowReceiver, EspNowSender, PeerInfo,
};
use esp_rtos::main;
use esp32_hid::mk_static;
use nb::block;
use smart_leds::{
    RGB8, SmartLedsWrite, brightness, gamma,
    hsv::{Hsv, hsv2rgb},
};

// This creates a default app-descriptor required by the esp-idf bootloader.
// For more information see: <https://docs.espressif.com/projects/esp-idf/en/stable/esp32/api-reference/system/app_image_format.html#application-description>
esp_bootloader_esp_idf::esp_app_desc!();
struct KeyMessage {
    press: bool,
    key: u8,
}

impl KeyMessage {
    pub fn to_bytes(&self) -> [u8; 3] {
        [self.press as u8, self.key, 0]
    }
    pub fn from_bytes(bytes: &[u8]) -> Option<Self> {
        if bytes.len() < 2 {
            return None;
        }
        Some(Self {
            press: bytes[0] != 0,
            key: bytes[1],
        })
    }
}

struct MultiKeyMessage {
    press: bool,
    key_1: u8,
    key_2: u8,
}

impl MultiKeyMessage {
    pub fn to_bytes(&self) -> [u8; 3] {
        [self.press as u8, self.key_1, self.key_2]
    }
    pub fn from_bytes(bytes: &[u8]) -> Option<Self> {
        if bytes.len() < 3 {
            return None;
        }
        Some(Self {
            press: bytes[0] != 0,
            key_1: bytes[1],
            key_2: bytes[2],
        })
    }
}

struct LayerMessage {
    new_layer: u8,
}

impl LayerMessage {
    pub fn to_bytes(&self) -> [u8; 3] {
        [self.new_layer as u8, 0, 0]
    }
    pub fn from_bytes(bytes: &[u8]) -> Option<Self> {
        if bytes.len() < 1 {
            return None;
        }
        Some(Self {
            new_layer: bytes[0],
        })
    }
}

enum GeneralMessage {
    KeyMessage(KeyMessage),
    LayerMessage(LayerMessage),
    MultiKeyMessage(MultiKeyMessage),
}

impl GeneralMessage {
    pub fn to_bytes(&self) -> [u8; 4] {
        match self {
            GeneralMessage::KeyMessage(m) => [0, m.to_bytes()[0], m.to_bytes()[1], m.to_bytes()[2]],
            GeneralMessage::LayerMessage(m) => {
                [1, m.to_bytes()[0], m.to_bytes()[1], m.to_bytes()[2]]
            }
            GeneralMessage::MultiKeyMessage(m) => {
                [2, m.to_bytes()[0], m.to_bytes()[1], m.to_bytes()[2]]
            }
        }
    }
    pub fn from_bytes(bytes: &[u8]) -> Option<Self> {
        if bytes.len() < 4 {
            return None;
        }
        return match bytes[0] {
            0 => Some(GeneralMessage::KeyMessage(
                KeyMessage::from_bytes(&bytes[1..]).unwrap(),
            )),
            1 => Some(GeneralMessage::LayerMessage(
                LayerMessage::from_bytes(&bytes[1..]).unwrap(),
            )),
            2 => Some(GeneralMessage::MultiKeyMessage(
                MultiKeyMessage::from_bytes(&bytes[1..]).unwrap(),
            )),
            _ => None,
        };
    }
}

enum KeyAction<'a> {
    multi_key(&'a [u8]),
    key(u8),
    layer_mo(u8),
}

static LED_SIGNAL: Signal<CriticalSectionRawMutex, GeneralMessage> = Signal::new();

#[main]
async fn main(spawner: Spawner) {
    let mut layer = 0;
    let peripherals = esp_hal::init(esp_hal::Config::default().with_cpu_clock(CpuClock::max()));

    // Configure a global allocator
    esp_alloc::heap_allocator!(size: 160 * 1024);

    // Configure RMT (Remote Control Transceiver) peripheral globally
    // <https://docs.espressif.com/projects/esp-idf/en/stable/esp32/api-reference/peripherals/rmt.html>
    let rmt: Rmt<'_, esp_hal::Blocking> = {
        let frequency: Rate = { Rate::from_mhz(80) };
        Rmt::new(peripherals.RMT, frequency)
    }
    .expect("Failed to initialize RMT");

    let rmt_channel = rmt.channel0;
    let mut rmt_buffer = smart_led_buffer!(21);

    let mut led = SmartLedsAdapter::new(rmt_channel, peripherals.GPIO37, &mut rmt_buffer);

    // Setup Embassy
    // (RTOS required for radio and async)
    // https://docs.espressif.com/projects/rust/esp-rtos/0.1.0/esp32/esp_rtos/index.html
    let timg0 = TimerGroup::new(peripherals.TIMG0);
    esp_rtos::start(timg0.timer0);

    // Setup watchdog on TIMG1, which is by default disabled by the bootloader
    // Watchdog resets system if not fed once per second
    let wdt1 = mk_static!(Wdt<TIMG1>, TimerGroup::new(peripherals.TIMG1).wdt);
    wdt1.set_timeout(MwdtStage::Stage0, esp_hal::time::Duration::from_secs(1));
    wdt1.set_stage_action(MwdtStage::Stage0, MwdtStageAction::ResetSystem);
    wdt1.enable();
    wdt1.feed();

    let esp_radio_ctrl = &*mk_static!(Controller<'static>, esp_radio::init().unwrap());
    let wifi = peripherals.WIFI;
    let (mut controller, interfaces) =
        esp_radio::wifi::new(&esp_radio_ctrl, wifi, Default::default()).unwrap();
    controller.set_mode(esp_radio::wifi::WifiMode::Sta).unwrap();
    controller.start().unwrap();
    let esp_now = interfaces.esp_now;
    esp_now.set_channel(11).unwrap();
    let (manager, sender, receiver) = esp_now.split();
    let manager = mk_static!(EspNowManager<'static>, manager);
    let sender = mk_static!(
        Mutex::<CriticalSectionRawMutex, EspNowSender<'static>>,
        Mutex::<CriticalSectionRawMutex, _>::new(sender)
    );
    spawner.must_spawn(listener(manager, receiver));
    spawner.must_spawn(broadcaster(sender));

    // Start watchdog task
    spawner.must_spawn(watchdog_task(wdt1));

    let mut color_red = Hsv {
        hue: 0,
        sat: 255,
        val: 255,
    };
    let mut color_100 = Hsv {
        hue: 100,
        sat: 255,
        val: 255,
    };
    let mut color_off = Hsv {
        hue: 100,
        sat: 255,
        val: 0,
    };
    let mut data_red: RGB8;
    let mut data_100: RGB8;
    let mut data_off: RGB8;

    data_red = hsv2rgb(color_red);
    data_100 = hsv2rgb(color_100);
    data_off = hsv2rgb(color_off);
    let level = 10;

    // Setup HID task
    // Uses USB GPIOs
    let usb = Usb::new(peripherals.USB0, peripherals.GPIO20, peripherals.GPIO19);
    let config = HidConfig::default();
    let mut keyboard = Keyboard::new(spawner, usb, config);

    // https://docs.espressif.com/projects/rust/esp-hal/1.0.0-beta.0/esp32/esp_hal/gpio/struct.Input.html
    let config = InputConfig::default().with_pull(Pull::Down);

    const NUM_KEYS: usize = 21;
    let keyswitch_arr: [esp_hal::gpio::Input; NUM_KEYS] = [
        Input::new(peripherals.GPIO2, config),
        Input::new(peripherals.GPIO3, config),
        Input::new(peripherals.GPIO4, config),
        Input::new(peripherals.GPIO5, config),
        Input::new(peripherals.GPIO6, config),
        Input::new(peripherals.GPIO7, config),
        Input::new(peripherals.GPIO8, config),
        Input::new(peripherals.GPIO9, config),
        Input::new(peripherals.GPIO10, config),
        Input::new(peripherals.GPIO11, config),
        Input::new(peripherals.GPIO12, config),
        Input::new(peripherals.GPIO13, config),
        Input::new(peripherals.GPIO14, config),
        Input::new(peripherals.GPIO17, config),
        Input::new(peripherals.GPIO18, config),
        Input::new(peripherals.GPIO21, config),
        Input::new(peripherals.GPIO38, config),
        Input::new(peripherals.GPIO45, config),
        Input::new(peripherals.GPIO46, config),
        Input::new(peripherals.GPIO47, config),
        Input::new(peripherals.GPIO48, config),
    ];

    let config = InputConfig::default().with_pull(Pull::Up);
    let pgood = Input::new(peripherals.GPIO35, config);
    let key_to_led = [
        18, 10, 5, 4, 3, 2, 11, 8, 7, 6, 17, 16, 15, 1, 0, 14, 19, 20, 9, 13, 12,
    ];
    let mut config: AdcConfig<ADC1> = AdcConfig::new();

    let mut pin = config.enable_pin(peripherals.GPIO1, Attenuation::_11dB);
    let mut adc1 = Adc::new(peripherals.ADC1, config);

    let mut led_color_arr = [data_off; NUM_KEYS];

    #[cfg(not(feature = "left"))]
    let layer_1 = [
        [
            [
                KeyAction::key(keycodes::HID_KEY_NONE),
                KeyAction::key(keycodes::HID_KEY_P),
                KeyAction::key(keycodes::HID_KEY_O),
                KeyAction::key(keycodes::HID_KEY_I),
                KeyAction::key(keycodes::HID_KEY_U),
                KeyAction::key(keycodes::HID_KEY_Y),
            ],
            [
                KeyAction::key(keycodes::HID_KEY_BACKSPACE),
                KeyAction::key(keycodes::HID_KEY_SEMICOLON),
                KeyAction::key(keycodes::HID_KEY_L),
                KeyAction::key(keycodes::HID_KEY_K),
                KeyAction::key(keycodes::HID_KEY_J),
                KeyAction::key(keycodes::HID_KEY_H),
            ],
            [
                KeyAction::key(keycodes::HID_KEY_NONE),
                KeyAction::key(keycodes::HID_KEY_NONE),
                KeyAction::key(keycodes::HID_KEY_PERIOD),
                KeyAction::key(keycodes::HID_KEY_COMMA),
                KeyAction::key(keycodes::HID_KEY_M),
                KeyAction::key(keycodes::HID_KEY_N),
            ],
            [
                KeyAction::key(keycodes::HID_KEY_NONE),
                KeyAction::key(keycodes::HID_KEY_NONE),
                KeyAction::key(keycodes::HID_KEY_NONE),
                KeyAction::key(keycodes::HID_KEY_SHIFT_LEFT),
                KeyAction::key(keycodes::HID_KEY_ENTER),
                KeyAction::layer_mo(2),
            ],
        ],
        [
            [
                KeyAction::key(keycodes::HID_KEY_NONE),
                KeyAction::key(keycodes::HID_KEY_NONE),
                KeyAction::key(keycodes::HID_KEY_NONE),
                KeyAction::key(keycodes::HID_KEY_TAB),
                KeyAction::key(keycodes::HID_KEY_ALT_LEFT),
                KeyAction::key(keycodes::HID_KEY_NONE),
            ],
            [
                KeyAction::key(keycodes::HID_KEY_BACKSPACE),
                KeyAction::key(keycodes::HID_KEY_ARROW_RIGHT),
                KeyAction::key(keycodes::HID_KEY_ARROW_UP),
                KeyAction::key(keycodes::HID_KEY_ARROW_DOWN),
                KeyAction::key(keycodes::HID_KEY_ARROW_LEFT),
                KeyAction::key(keycodes::HID_KEY_GUI_LEFT),
            ],
            [
                KeyAction::key(keycodes::HID_KEY_NONE),
                KeyAction::key(keycodes::HID_KEY_END),
                KeyAction::key(keycodes::HID_KEY_PRINT_SCREEN),
                KeyAction::key(keycodes::HID_KEY_NONE),
                KeyAction::key(keycodes::HID_KEY_HOME),
                KeyAction::key(keycodes::HID_KEY_NONE),
            ],
            [
                KeyAction::key(keycodes::HID_KEY_NONE),
                KeyAction::key(keycodes::HID_KEY_NONE),
                KeyAction::key(keycodes::HID_KEY_NONE),
                KeyAction::key(keycodes::HID_KEY_NONE),
                KeyAction::key(keycodes::HID_KEY_NONE),
                KeyAction::layer_mo(2),
            ],
        ],
        [
            [
                KeyAction::key(keycodes::HID_KEY_NONE),
                KeyAction::multi_key(&[keycodes::HID_KEY_SHIFT_LEFT, keycodes::HID_KEY_7]),
                KeyAction::key(keycodes::HID_KEY_EQUAL),
                KeyAction::multi_key(&[keycodes::HID_KEY_SHIFT_LEFT, keycodes::HID_KEY_PERIOD]),
                KeyAction::multi_key(&[keycodes::HID_KEY_SHIFT_LEFT, keycodes::HID_KEY_COMMA]),
                KeyAction::multi_key(&[keycodes::HID_KEY_SHIFT_LEFT, keycodes::HID_KEY_1]),
            ],
            [
                KeyAction::key(keycodes::HID_KEY_BACKSPACE),
                KeyAction::multi_key(&[keycodes::HID_KEY_SHIFT_LEFT, keycodes::HID_KEY_SEMICOLON]),
                KeyAction::key(keycodes::HID_KEY_APOSTROPHE),
                KeyAction::multi_key(&[keycodes::HID_KEY_SHIFT_LEFT, keycodes::HID_KEY_0]),
                KeyAction::multi_key(&[keycodes::HID_KEY_SHIFT_LEFT, keycodes::HID_KEY_9]),
                KeyAction::multi_key(&[keycodes::HID_KEY_SHIFT_LEFT, keycodes::HID_KEY_SLASH]),
            ],
            [
                KeyAction::key(keycodes::HID_KEY_NONE),
                KeyAction::key(keycodes::HID_KEY_NONE),
                KeyAction::multi_key(&[keycodes::HID_KEY_SHIFT_LEFT, keycodes::HID_KEY_2]),
                KeyAction::key(keycodes::HID_KEY_BACKSLASH),
                KeyAction::multi_key(&[keycodes::HID_KEY_SHIFT_LEFT, keycodes::HID_KEY_5]),
                KeyAction::multi_key(&[keycodes::HID_KEY_SHIFT_LEFT, keycodes::HID_KEY_EQUAL]),
            ],
            [
                KeyAction::key(keycodes::HID_KEY_NONE),
                KeyAction::key(keycodes::HID_KEY_NONE),
                KeyAction::key(keycodes::HID_KEY_NONE),
                KeyAction::key(keycodes::HID_KEY_NONE),
                KeyAction::key(keycodes::HID_KEY_NONE),
                KeyAction::layer_mo(2),
            ],
        ],
    ];

    #[cfg(feature = "left")]
    let layer_1 = [
        [
            [
                KeyAction::key(keycodes::HID_KEY_ESCAPE),
                KeyAction::key(keycodes::HID_KEY_Q),
                KeyAction::key(keycodes::HID_KEY_W),
                KeyAction::key(keycodes::HID_KEY_E),
                KeyAction::key(keycodes::HID_KEY_R),
                KeyAction::key(keycodes::HID_KEY_T),
            ],
            [
                KeyAction::key(keycodes::HID_KEY_TAB),
                KeyAction::key(keycodes::HID_KEY_A),
                KeyAction::key(keycodes::HID_KEY_S),
                KeyAction::key(keycodes::HID_KEY_D),
                KeyAction::key(keycodes::HID_KEY_F),
                KeyAction::key(keycodes::HID_KEY_G),
            ],
            [
                KeyAction::key(keycodes::HID_KEY_NONE),
                KeyAction::key(keycodes::HID_KEY_Z),
                KeyAction::key(keycodes::HID_KEY_X),
                KeyAction::key(keycodes::HID_KEY_C),
                KeyAction::key(keycodes::HID_KEY_V),
                KeyAction::key(keycodes::HID_KEY_B),
            ],
            [
                KeyAction::key(keycodes::HID_KEY_NONE),
                KeyAction::key(keycodes::HID_KEY_NONE),
                KeyAction::key(keycodes::HID_KEY_NONE),
                KeyAction::key(keycodes::HID_KEY_CONTROL_LEFT),
                KeyAction::key(keycodes::HID_KEY_SPACE),
                KeyAction::layer_mo(1),
            ],
        ],
        [
            [
                KeyAction::key(keycodes::HID_KEY_ESCAPE),
                KeyAction::key(keycodes::HID_KEY_NONE),
                KeyAction::key(keycodes::HID_KEY_7),
                KeyAction::key(keycodes::HID_KEY_8),
                KeyAction::key(keycodes::HID_KEY_9),
                KeyAction::key(keycodes::HID_KEY_NONE),
            ],
            [
                KeyAction::key(keycodes::HID_KEY_TAB),
                KeyAction::key(keycodes::HID_KEY_0),
                KeyAction::key(keycodes::HID_KEY_4),
                KeyAction::key(keycodes::HID_KEY_5),
                KeyAction::key(keycodes::HID_KEY_6),
                KeyAction::key(keycodes::HID_KEY_ENTER),
            ],
            [
                KeyAction::key(keycodes::HID_KEY_NONE),
                KeyAction::key(keycodes::HID_KEY_NONE),
                KeyAction::key(keycodes::HID_KEY_1),
                KeyAction::key(keycodes::HID_KEY_2),
                KeyAction::key(keycodes::HID_KEY_3),
                KeyAction::key(keycodes::HID_KEY_PERIOD),
            ],
            [
                KeyAction::key(keycodes::HID_KEY_NONE),
                KeyAction::key(keycodes::HID_KEY_NONE),
                KeyAction::key(keycodes::HID_KEY_NONE),
                KeyAction::key(keycodes::HID_KEY_NONE),
                KeyAction::key(keycodes::HID_KEY_NONE),
                KeyAction::layer_mo(1),
            ],
        ],
        [
            [
                KeyAction::key(keycodes::HID_KEY_ESCAPE),
                KeyAction::multi_key(&[keycodes::HID_KEY_SHIFT_LEFT, keycodes::HID_KEY_APOSTROPHE]),
                KeyAction::multi_key(&[keycodes::HID_KEY_SHIFT_LEFT, keycodes::HID_KEY_MINUS]),
                KeyAction::key(keycodes::HID_KEY_BRACKET_LEFT),
                KeyAction::key(keycodes::HID_KEY_BRACKET_RIGHT),
                KeyAction::multi_key(&[keycodes::HID_KEY_SHIFT_LEFT, keycodes::HID_KEY_6]),
            ],
            [
                KeyAction::key(keycodes::HID_KEY_NONE),
                KeyAction::key(keycodes::HID_KEY_SLASH),
                KeyAction::key(keycodes::HID_KEY_MINUS),
                KeyAction::multi_key(&[
                    keycodes::HID_KEY_SHIFT_LEFT,
                    keycodes::HID_KEY_BRACKET_LEFT,
                ]),
                KeyAction::multi_key(&[
                    keycodes::HID_KEY_SHIFT_LEFT,
                    keycodes::HID_KEY_BRACKET_RIGHT,
                ]),
                KeyAction::multi_key(&[keycodes::HID_KEY_SHIFT_LEFT, keycodes::HID_KEY_8]),
            ],
            [
                KeyAction::key(keycodes::HID_KEY_NONE),
                KeyAction::multi_key(&[keycodes::HID_KEY_SHIFT_LEFT, keycodes::HID_KEY_3]),
                KeyAction::multi_key(&[keycodes::HID_KEY_SHIFT_LEFT, keycodes::HID_KEY_4]),
                KeyAction::multi_key(&[keycodes::HID_KEY_SHIFT_LEFT, keycodes::HID_KEY_BACKSLASH]),
                KeyAction::multi_key(&[keycodes::HID_KEY_SHIFT_LEFT, keycodes::HID_KEY_GRAVE]),
                KeyAction::key(keycodes::HID_KEY_GRAVE),
            ],
            [
                KeyAction::key(keycodes::HID_KEY_NONE),
                KeyAction::key(keycodes::HID_KEY_NONE),
                KeyAction::key(keycodes::HID_KEY_NONE),
                KeyAction::key(keycodes::HID_KEY_NONE),
                KeyAction::key(keycodes::HID_KEY_NONE),
                KeyAction::layer_mo(1),
            ],
        ],
    ];

    let led_matrix = [
        (5, 0),
        (4, 0),
        (3, 0),
        (2, 0),
        (1, 0),
        (0, 0),
        (5, 1),
        (4, 1),
        (3, 1),
        (2, 1),
        (1, 1),
        (0, 1),
        (5, 2),
        (4, 2),
        (3, 2),
        (2, 2),
        (1, 2),
        (0, 2),
        (5, 3),
        (4, 3),
        (3, 3),
    ];
    let key_matrix = [
        (5, 3),
        (1, 1),
        (0, 0),
        (1, 0),
        (2, 0),
        (3, 0),
        (0, 1),
        (3, 1),
        (4, 1),
        (5, 1),
        (0, 2),
        (1, 2),
        (2, 2),
        (4, 0),
        (5, 0),
        (3, 2),
        (4, 3),
        (3, 3),
        (2, 1),
        (4, 2),
        (5, 2),
    ];

    let mut keyswitch_pressed: [bool; NUM_KEYS] = [false; NUM_KEYS];
    let mut percent = 0;
    loop {
        percent += 1;
        if percent > 100 {
            percent = 0;
        }
        let pos = block!(adc1.read_oneshot(&mut pin)).unwrap() as f64 / 400.0;
        // let pos = (percent / 10) - 2;
        for i in 0..NUM_KEYS {
            if keyswitch_arr[i].is_high() && !keyswitch_pressed[i] {
                keyswitch_pressed[i] = true;
                if let KeyAction::key(key) = layer_1[layer][key_matrix[i].1][key_matrix[i].0] {
                    keyboard.press(key).await;
                    let peer = manager.fetch_peer(true);
                    if peer.is_ok() {
                        let k = KeyMessage {
                            press: true,
                            key: key,
                        };
                        let g = GeneralMessage::KeyMessage(k);
                        let msg = GeneralMessage::to_bytes(&g);
                        let mut sender = sender.lock().await;
                        let status = sender.send_async(&peer.unwrap().peer_address, &msg).await;
                    }
                } else if let KeyAction::layer_mo(l) =
                    layer_1[layer][key_matrix[i].1][key_matrix[i].0]
                {
                    layer = l as usize;
                    keyboard.release(keycodes::HID_KEY_SHIFT_LEFT).await;
                    let peer = manager.fetch_peer(true);
                    if peer.is_ok() {
                        let k = LayerMessage {
                            new_layer: layer as u8,
                        };
                        let g = GeneralMessage::LayerMessage(k);
                        let msg = GeneralMessage::to_bytes(&g);
                        let mut sender = sender.lock().await;
                        let status = sender.send_async(&peer.unwrap().peer_address, &msg).await;
                    }
                } else if let KeyAction::multi_key(k) =
                    layer_1[layer][key_matrix[i].1][key_matrix[i].0]
                {
                    for key in k {
                        keyboard.press(*key).await;
                    }
                    let peer = manager.fetch_peer(true);
                    if peer.is_ok() {
                        let m = MultiKeyMessage {
                            press: true,
                            key_1: k[0],
                            key_2: k[1],
                        };
                        let g = GeneralMessage::MultiKeyMessage(m);
                        let msg = GeneralMessage::to_bytes(&g);
                        let mut sender = sender.lock().await;
                        let status = sender.send_async(&peer.unwrap().peer_address, &msg).await;
                    }
                }
            }
            if keyswitch_arr[i].is_low() && keyswitch_pressed[i] {
                keyswitch_pressed[i] = false;
                if let KeyAction::key(key) = layer_1[layer][key_matrix[i].1][key_matrix[i].0] {
                    keyboard.release(key).await;
                    let peer = manager.fetch_peer(true);
                    if peer.is_ok() {
                        let k = KeyMessage {
                            press: false,
                            key: key,
                        };
                        let g = GeneralMessage::KeyMessage(k);
                        let msg = GeneralMessage::to_bytes(&g);
                        let mut sender = sender.lock().await;
                        let status = sender.send_async(&peer.unwrap().peer_address, &msg).await;
                    }
                } else if let KeyAction::layer_mo(l) =
                    layer_1[layer][key_matrix[i].1][key_matrix[i].0]
                {
                    layer = 0;
                    keyboard.release(keycodes::HID_KEY_SHIFT_LEFT).await;
                    let peer = manager.fetch_peer(true);
                    if peer.is_ok() {
                        let k = LayerMessage {
                            new_layer: layer as u8,
                        };
                        let g = GeneralMessage::LayerMessage(k);
                        let msg = GeneralMessage::to_bytes(&g);
                        let mut sender = sender.lock().await;
                        let status = sender.send_async(&peer.unwrap().peer_address, &msg).await;
                    }
                } else if let KeyAction::multi_key(k) =
                    layer_1[layer][key_matrix[i].1][key_matrix[i].0]
                {
                    for key in k {
                        keyboard.release(*key).await;
                    }
                    let peer = manager.fetch_peer(true);
                    if peer.is_ok() {
                        let m = MultiKeyMessage {
                            press: false,
                            key_1: k[0],
                            key_2: k[1],
                        };
                        let g = GeneralMessage::MultiKeyMessage(m);
                        let msg = GeneralMessage::to_bytes(&g);
                        let mut sender = sender.lock().await;
                        let status = sender.send_async(&peer.unwrap().peer_address, &msg).await;
                    }
                }
            }

            // if ((led_matrix[i].0 - pos) as i32).abs() < 1 {
            //     led_color_arr[i] = data_100;
            // } else {
            //     led_color_arr[i] = data_red;
            // }

            //if keyswitch_pressed[i] {
            //    led_color_arr[key_to_led[i]] = data_100;
            //} else {
            //    led_color_arr[key_to_led[i]] = data_off;
            //}

            // if pgood.is_high(){
            //     led_color_arr[i] = data_100;
            // } else {
            //     led_color_arr[i] = data_red;
            // }

            // if (i as f64) < pos {
            //     led_color_arr[i] = data_100;
            // } else {
            //     led_color_arr[i] = data_red;
            // }
        }

        if let Some(new_colors) = LED_SIGNAL.try_take() {
            if let GeneralMessage::KeyMessage(km) = new_colors {
                if km.press {
                    keyboard.press(km.key).await;
                } else {
                    keyboard.release(km.key).await;
                }
            } else if let GeneralMessage::LayerMessage(lm) = new_colors {
                layer = lm.new_layer as usize;
                    keyboard.release(keycodes::HID_KEY_SHIFT_LEFT).await;
            } else if let GeneralMessage::MultiKeyMessage(km) = new_colors {
                if km.press {
                    keyboard.press(km.key_1).await;
                    keyboard.press(km.key_2).await;
                } else {
                    keyboard.release(km.key_1).await;
                    keyboard.release(km.key_2).await;
                }
            }
        }

        led.write(brightness(gamma(led_color_arr.into_iter()), level))
            .unwrap();
        // Yield here is required. Without it, there is significant lag, presumably because the HID task doesn't get adequate runtime
        Timer::after(Duration::from_millis(5)).await;
    }
}

// Watchdog that gets fed every 500 ms
#[embassy_executor::task]
async fn watchdog_task(watchdog: &'static mut Wdt<TIMG1<'static>>) {
    loop {
        watchdog.feed();
        Timer::after(Duration::from_millis(500)).await;
    }
}

#[embassy_executor::task]
async fn broadcaster(sender: &'static Mutex<CriticalSectionRawMutex, EspNowSender<'static>>) {
    loop {
        Timer::after(Duration::from_millis(500)).await;

        let mut sender = sender.lock().await;
        let status = sender.send_async(&BROADCAST_ADDRESS, b"Hello.").await;
    }
}

#[embassy_executor::task]
async fn listener(manager: &'static EspNowManager<'static>, mut receiver: EspNowReceiver<'static>) {
    let mac = Efuse::mac_address();
    loop {
        let r = receiver.receive_async().await;
        if r.info.dst_address == BROADCAST_ADDRESS {
            if !manager.peer_exists(&r.info.src_address) {
                manager
                    .add_peer(PeerInfo {
                        interface: esp_radio::esp_now::EspNowWifiInterface::Sta,
                        peer_address: r.info.src_address,
                        lmk: None,
                        channel: None,
                        encrypt: false,
                    })
                    .unwrap();
            }
        } else if r.info.dst_address == mac {
            if let Some(msg) = GeneralMessage::from_bytes(r.data()) {
                LED_SIGNAL.signal(msg);
            }
        }
    }
}
