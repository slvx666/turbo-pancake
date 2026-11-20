//! turbo-pancake desktop client logic, kept free of UI types so it can be tested
//! headless: [`session`] (signaling + audio engine + 1:1 call) and [`view`] (status text).

pub mod alerts;
pub mod app_settings;
pub mod banner;
pub mod chat;
pub mod credentials;
pub mod instance;
pub mod opaque;
pub mod screen;
pub mod session;
pub mod sounds;
pub mod taskbar;
pub mod tray;
pub mod ui_scale;
pub mod updater;
pub mod video_surface;
pub mod view;
