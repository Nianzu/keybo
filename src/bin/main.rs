#![no_std]
#![no_main]

use esp32_hid::{hid_config::HidConfig, keyboard::Keyboard, keycodes};
extern crate alloc;
use embassy_executor::Spawner;
use embassy_time::{Duration, Timer};
use esp_backtrace as _;
use esp_hal::{
    clock::CpuClock,
    gpio::{Input, InputConfig, Pull},
    otg_fs::Usb,
    peripherals::TIMG1,
    timer::timg::{MwdtStage, MwdtStageAction, TimerGroup, Wdt},
};
use esp_rtos::main;
use esp32_hid::mk_static;

// This creates a default app-descriptor required by the esp-idf bootloader.
// For more information see: <https://docs.espressif.com/projects/esp-idf/en/stable/esp32/api-reference/system/app_image_format.html#application-description>
esp_bootloader_esp_idf::esp_app_desc!();

#[main]
async fn main(spawner: Spawner) {
    let peripherals = esp_hal::init(esp_hal::Config::default().with_cpu_clock(CpuClock::max()));

    // Configure a global allocator
    esp_alloc::heap_allocator!(size: 160 * 1024);

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

    // Start watchdog task
    spawner.must_spawn(watchdog_task(wdt1));

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
    let keycode_arr: [u8; NUM_KEYS] = [
        keycodes::HID_KEY_A, 
        keycodes::HID_KEY_B, 
        keycodes::HID_KEY_C, 
        keycodes::HID_KEY_D, 
        keycodes::HID_KEY_E, 
        keycodes::HID_KEY_F, 
        keycodes::HID_KEY_G, 
        keycodes::HID_KEY_H, 
        keycodes::HID_KEY_I, 
        keycodes::HID_KEY_J, 
        keycodes::HID_KEY_K, 
        keycodes::HID_KEY_L, 
        keycodes::HID_KEY_M, 
        keycodes::HID_KEY_N, 
        keycodes::HID_KEY_O, 
        keycodes::HID_KEY_P, 
        keycodes::HID_KEY_Q, 
        keycodes::HID_KEY_R, 
        keycodes::HID_KEY_S, 
        keycodes::HID_KEY_T, 
        keycodes::HID_KEY_U, 
    ];
    let mut keyswitch_pressed: [bool; NUM_KEYS] = [false; NUM_KEYS];
    loop {
        for i in ttttttttttttttttttttttttttttttttttttttttttttttttttttttttttttttttttttttttttttttttttttttttttttttttttttrrrrrrrrrrrrrrrrrrrrrrrrrrrqqqqqqqqqr0..NUM_KEYS {
            if keyswitch_arr[i].is_high() && !keyswitch_pressed[i] {
                keyswitch_pressed[i] = true;
                keyboard.press(keycode_arr[i]).await;
            }
            if keyswitch_arr[i].is_low() && keyswitch_pressed[i] {
                keyswitch_pressed[i] = false;
                keyboard.release(keycode_arr[i]).await;
            }
        }
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
