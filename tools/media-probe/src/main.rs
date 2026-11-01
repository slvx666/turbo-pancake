//! Enumerates audio hosts and devices with their default stream configs and
//! opens the default input and output for one second to prove the device can
//! actually be driven (callback count is reported). Output is JSON.
//!
//! Usage: `cargo run -p dsmini-media-probe [-- --no-open]`

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use serde_json::{Value, json};

fn config_json<E: std::fmt::Display>(c: Result<cpal::SupportedStreamConfig, E>) -> Value {
    match c {
        Ok(c) => json!({
            "sample_rate": c.sample_rate(),
            "channels": c.channels(),
            "sample_format": format!("{:?}", c.sample_format()),
            "buffer_size": format!("{:?}", c.buffer_size()),
        }),
        Err(e) => json!({ "error": e.to_string() }),
    }
}

fn device_name(d: &cpal::Device) -> String {
    d.description()
        .map(|desc| desc.name().to_owned())
        .unwrap_or_else(|e| format!("<unnamed: {e}>"))
}

/// Opens the device for `secs` and counts data callbacks.
fn open_test(device: &cpal::Device, input: bool) -> Value {
    let cfg = if input {
        device.default_input_config()
    } else {
        device.default_output_config()
    };
    let cfg = match cfg {
        Ok(c) => c,
        Err(e) => return json!({ "error": e.to_string() }),
    };
    let calls = Arc::new(AtomicU64::new(0));
    let frames = Arc::new(AtomicU64::new(0));
    let (c2, f2) = (calls.clone(), frames.clone());
    let channels = u64::from(cfg.channels());
    let err_fn = |e| eprintln!("stream error: {e}");
    let stream = if input {
        device.build_input_stream(
            cfg.config(),
            move |data: &[f32], _: &cpal::InputCallbackInfo| {
                c2.fetch_add(1, Ordering::Relaxed);
                f2.fetch_add(data.len() as u64 / channels, Ordering::Relaxed);
            },
            err_fn,
            None,
        )
    } else {
        device.build_output_stream(
            cfg.config(),
            move |data: &mut [f32], _: &cpal::OutputCallbackInfo| {
                // Silence: the probe must never make noise.
                data.fill(0.0);
                c2.fetch_add(1, Ordering::Relaxed);
                f2.fetch_add(data.len() as u64 / channels, Ordering::Relaxed);
            },
            err_fn,
            None,
        )
    };
    let stream = match stream {
        Ok(s) => s,
        Err(e) => return json!({ "error": e.to_string(), "note": "f32 stream build failed" }),
    };
    if let Err(e) = stream.play() {
        return json!({ "error": e.to_string() });
    }
    std::thread::sleep(Duration::from_secs(1));
    drop(stream);
    json!({
        "callbacks_per_sec": calls.load(Ordering::Relaxed),
        "frames_per_sec": frames.load(Ordering::Relaxed),
        "expected_frames_per_sec": cfg.sample_rate(),
    })
}

fn main() -> anyhow::Result<()> {
    let open = !std::env::args().any(|a| a == "--no-open");
    let mut hosts = Vec::new();
    for id in cpal::available_hosts() {
        let host = match cpal::host_from_id(id) {
            Ok(h) => h,
            Err(e) => {
                hosts.push(json!({ "host": format!("{id:?}"), "error": e.to_string() }));
                continue;
            }
        };
        let inputs: Vec<Value> = host
            .input_devices()
            .map(|it| {
                it.map(|d| json!({ "name": device_name(&d), "default": config_json(d.default_input_config()) }))
                    .collect()
            })
            .unwrap_or_default();
        let outputs: Vec<Value> = host
            .output_devices()
            .map(|it| {
                it.map(|d| json!({ "name": device_name(&d), "default": config_json(d.default_output_config()) }))
                    .collect()
            })
            .unwrap_or_default();
        hosts.push(json!({
            "host": format!("{id:?}"),
            "default_input": host.default_input_device().map(|d| device_name(&d)),
            "default_output": host.default_output_device().map(|d| device_name(&d)),
            "inputs": inputs,
            "outputs": outputs,
        }));
    }

    let host = cpal::default_host();
    let open_results = if open {
        json!({
            "default_input": host.default_input_device().map(|d| open_test(&d, true)),
            "default_output": host.default_output_device().map(|d| open_test(&d, false)),
        })
    } else {
        Value::Null
    };

    println!(
        "{}",
        serde_json::to_string_pretty(&json!({
            "default_host": format!("{:?}", host.id()),
            "hosts": hosts,
            "open_test": open_results,
        }))?
    );
    Ok(())
}
