//! turbo-pancake desktop client logic, kept free of UI types so it can be tested
//! headless: [`session`] (signaling + audio engine + 1:1 call) and [`view`] (status text).

pub mod screen;
pub mod session;
pub mod view;
