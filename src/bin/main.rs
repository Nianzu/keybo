#![no_std]
#![no_main]

use esp32_hid::{hid_config::HidConfig, keyboard::Keyboard, keycodes};
extern crate alloc;
use embassy_executor::Spawner;
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::{blocking_mutex::raw::NoopRawMutex, mutex::Mutex, signal::Signal};
use embassy_time::{Duration, Timer};
use esp_backtrace as _;
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
use esp_radio::esp_now::{
    BROADCAST_ADDRESS, EspNowManager, EspNowReceiver, EspNowSender, PeerInfo,
};
use esp_radio::Controller;
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
static LED_SIGNAL: Signal<CriticalSectionRawMutex, [RGB8; 21]> = Signal::new();

#[main]
async fn main(spawner: Spawner) {
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

    // https://github.com/esp-rs/esp-hal/blob/main/examples/esp-now/embassy_esp_now_duplex/src/main.rs
    // start the controller in station mode

    let esp_radio_ctrl = &*mk_static!(Controller<'static>, esp_radio::init().unwrap());

    let wifi = peripherals.WIFI;
    let (mut controller, interfaces) =
        esp_radio::wifi::new(&esp_radio_ctrl, wifi, Default::default()).unwrap();
    controller.set_mode(esp_radio::wifi::WifiMode::Sta).unwrap();
    controller.start().unwrap();
    let esp_now = interfaces.esp_now;

// Note: pass a reference to the controller, not ownership

esp_now.set_channel(11).unwrap();

let (manager, sender, receiver) = esp_now.split();
let manager = mk_static!(EspNowManager<'static>, manager);
let sender = mk_static!(
    Mutex::<CriticalSectionRawMutex, EspNowSender<'static>>,
    Mutex::<CriticalSectionRawMutex, _>::new(sender)
);

spawner.must_spawn(listener(manager, receiver));
spawner.must_spawn(broadcaster(sender));
    // let (_wifi_controller, interfaces) =
    //     esp_radio::wifi::new(radio_controller, wifi, Default::default()).unwrap();

    // let (_controller, interfaces) = esp_radio::wifi::new(wifi, Default::default()).unwrap();
    // let esp_now = interfaces.esp_now;
    // esp_now.set_channel(11).unwrap();
    // let (manager, sender, receiver) = esp_now.split();
    // let manager = mk_static!(EspNowManager<'static>, manager);
    // let sender = mk_static!(
    //     Mutex::<CriticalSectionRawMutex, EspNowSender<'static>>,
    //     Mutex::<CriticalSectionRawMutex, _>::new(sender)
    // );
    // spawner.must_spawn(listener(manager, receiver));
    // spawner.must_spawn(broadcaster(sender));
    //spawner.spawn(listener(manager, receiver).unwrap());
    //spawner.spawn(broadcaster(sender).unwrap());

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
    let mut data_red: RGB8;
    let mut data_100: RGB8;
    data_red = hsv2rgb(color_red);
    data_100 = hsv2rgb(color_100);
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

    let mut led_color_arr = [data_red; NUM_KEYS];
    let layer_1 = [
        [
            keycodes::HID_KEY_A,
            keycodes::HID_KEY_B,
            keycodes::HID_KEY_C,
            keycodes::HID_KEY_D,
            keycodes::HID_KEY_E,
            keycodes::HID_KEY_F,
        ],
        [
            keycodes::HID_KEY_G,
            keycodes::HID_KEY_H,
            keycodes::HID_KEY_I,
            keycodes::HID_KEY_J,
            keycodes::HID_KEY_K,
            keycodes::HID_KEY_L,
        ],
        [
            keycodes::HID_KEY_M,
            keycodes::HID_KEY_N,
            keycodes::HID_KEY_O,
            keycodes::HID_KEY_P,
            keycodes::HID_KEY_Q,
            keycodes::HID_KEY_R,
        ],
        [
            keycodes::HID_KEY_S,
            keycodes::HID_KEY_T,
            keycodes::HID_KEY_U,
            keycodes::HID_KEY_V,
            keycodes::HID_KEY_W,
            keycodes::HID_KEY_X,
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
                keyboard
                    .press(layer_1[key_matrix[i].1][key_matrix[i].0])
                    .await;
            }
            if keyswitch_arr[i].is_low() && keyswitch_pressed[i] {
                keyswitch_pressed[i] = false;
                keyboard
                    .release(layer_1[key_matrix[i].1][key_matrix[i].0])
                    .await;
            }

            // if ((led_matrix[i].0 - pos) as i32).abs() < 1 {
            //     led_color_arr[i] = data_100;
            // } else {
            //     led_color_arr[i] = data_red;
            // }

            // if keyswitch_pressed[i] {
            //     led_color_arr[key_to_led[i]] = data_100;
            // } else{
            //     led_color_arr[key_to_led[i]] = data_red;
            // }

            // if pgood.is_high(){
            //     led_color_arr[i] = data_100;
            // } else {
            //     led_color_arr[i] = data_red;
            // }

            if (i as f64) < pos {
                led_color_arr[i] = data_100;
            } else {
                led_color_arr[i] = data_red;
            }
        }

        if let Some(new_colors) = LED_SIGNAL.try_take() {
            led_color_arr = new_colors;
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
    loop {
        let r = receiver.receive_async().await;
        let colors = [RGB8 { r: 0, g: 50, b: 0 }; 21];
        LED_SIGNAL.signal(colors);
        if r.info.dst_address == BROADCAST_ADDRESS {
            if !manager.peer_exists(&r.info.src_address) {
                manager
                    .add_peer(PeerInfo {
                        //interface: esp_radio::esp_now::EspNowWifiInterface::Station,
                        interface: esp_radio::esp_now::EspNowWifiInterface::Sta,
                        peer_address: r.info.src_address,
                        lmk: None,
                        channel: None,
                        encrypt: false,
                    })
                    .unwrap();
            }
        }
    }
}
