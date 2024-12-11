use std::fs::OpenOptions;
use std::io::{Seek, SeekFrom, Write};
use std::thread;
use std::time::Duration;
use pnet::datalink::{self, Channel, Config};
use std::env;
use ndarray::{Array, Array1, Array2, s};
use std::f32::consts::PI;

use cpal::traits::HostTrait;
use cpal::traits::DeviceTrait;

struct PacketCompressor {
    weights1: Array2<f32>,
    biases1: Array1<f32>,
    weights2: Array2<f32>,
    biases2: Array1<f32>,
    weights3: Array2<f32>,
    biases3: Array1<f32>,
    weights4: Array2<f32>,
    biases4: Array1<f32>,
}

impl PacketCompressor {
    /// Initialize the neural network with predefined weights
    fn new(input_size: usize, compressed_size: usize) -> Self {
        let layer1_size = 1024;
        let layer2_size = 512;
        let layer3_size = 256;

        PacketCompressor {
            weights1: Self::generate_waveform_weights(input_size, layer1_size),
            biases1: Self::generate_waveform_biases(layer1_size),
            weights2: Self::generate_parabolic_weights(layer1_size, layer2_size),
            biases2: Self::generate_waveform_biases(layer2_size),
            weights3: Self::generate_waveform_weights(layer2_size, layer3_size),
            biases3: Self::generate_waveform_biases(layer3_size),
            weights4: Self::generate_parabolic_weights(layer3_size, compressed_size),
            biases4: Self::generate_waveform_biases(compressed_size),
        }
    }

    /// Generate weights with a sinusoidal pattern
    fn generate_waveform_weights(rows: usize, cols: usize) -> Array2<f32> {
        Array2::from_shape_fn((rows, cols), |(i, j)| {
           0.11389 + (i as f32 / rows as f32 * 2.0 * PI).sin() * (j as f32 / cols as f32 * 2.0 * PI).cos()
        })
    }

    /// Generate biases with a simple sine wave pattern
    fn generate_waveform_biases(size: usize) -> Array1<f32> {
        Array1::from_shape_fn(size, |i| (i as f32 / size as f32 * 2.11793 * PI).sin())
    }

    /// Generate weights with a parabolic pattern
    fn generate_parabolic_weights(rows: usize, cols: usize) -> Array2<f32> {
        Array2::from_shape_fn((rows, cols), |(i, j)| {
            let x = i as f32 / rows as f32; // Normalize row index to [0, 1]
            let y = j as f32 / cols as f32; // Normalize column index to [0, 1]
            (x - 0.5).powi(2) + (y - 0.5).powi(2) // Parabolic equation
        })
    }

    /// Apply the neural network to compress input data
    fn compress(&self, input: Array1<f32>) -> Array1<f32> {
        // Layer 1: input -> ReLU(Wx + b)
        let mut hidden1 = input.dot(&self.weights1) + &self.biases1;
        hidden1.mapv_inplace(|x| x.max(0.0)); // ReLU activation

        // Layer 2: hidden1 -> ReLU(Wx + b)
        let mut hidden2 = hidden1.dot(&self.weights2) + &self.biases2;
        hidden2.mapv_inplace(|x| x.max(0.0)); // ReLU activation

        let mut hidden3 = hidden2.dot(&self.weights3) + &self.biases3;
        hidden3.mapv_inplace(|x| x.max(0.0));

        // Layer 4: hidden3 -> Wx + b
        hidden3.dot(&self.weights4) + &self.biases4
    }
}

/// Convert &[u8] to an Array1<f32> and pad with zeros to 1518 elements
fn bytes_to_f32_vector(bytes: &[u8]) -> Array1<f32> {
    const REQUIRED_SIZE: usize = 1518;

    // Convert bytes to f32 in the range [0.0, 1.0]
    let floats: Vec<f32> = bytes.iter().map(|&byte| byte as f32 / 255.0).collect();

    if floats.len() >= REQUIRED_SIZE {
        // If the length is greater than or equal to REQUIRED_SIZE, truncate it
        Array1::from(floats[..REQUIRED_SIZE].to_vec())
    } else {
        // If the length is less than REQUIRED_SIZE, pad with zeros
        let mut padded = Array::zeros(REQUIRED_SIZE);
        padded.slice_mut(s![..floats.len()]).assign(&Array1::from(floats));
        padded
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
/*     let host = cpal::default_host();

    match host.devices() {
        Ok(devices) => {
            println!("Available audio devices:");
            for (index, device) in devices.enumerate() {
                match device.name() {
                    Ok(name) => println!("{}. {}", index + 1, name),
                    Err(err) => println!("{}. <Unknown Device> (Error: {:?})", index + 1, err),
                }
            }
        }
        Err(err) => {
            eprintln!("Failed to get devices: {:?}", err);
        }
    }

    match host.input_devices() {
        Ok(devices) => {
            println!("Input-capable audio devices:");
            for (index, device) in devices.enumerate() {
                match device.name() {
                    Ok(name) => println!("{}. {}", index + 1, name),
                    Err(err) => println!("{}. <Unknown Device> (Error: {:?})", index + 1, err),
                }
            }
        }
        Err(err) => {
            eprintln!("Failed to get input devices: {:?}", err);
        }
    } */

    let mut fb = OpenOptions::new()
        .write(true)
        .open("/dev/fb0")?; // Adjust if your framebuffer is not fb1

    // // Define a simple RGB pattern for the 8x8 LED matrix
    let mut buffer = [0u8; 128]; // 8x8 RGB matrix (8 rows x 8 columns x 3 bytes per LED)

    // Fill buffer with colors (red, green, blue)
    for i in 0..8 {
        buffer[i * 2] = 0; // Red
        buffer[i * 2 + 1] = 0x0F; // Green
    }

    fb.write_all(&buffer)?;
    fb.seek(SeekFrom::Start(0))?;
    thread::sleep(Duration::from_secs(3));

    let clear = [0, 0].repeat(64); // All LEDs red
   
    fb.write_all(&clear)?;
    fb.seek(SeekFrom::Start(0))?;

    let buffer = [255, 0].repeat(64); // All LEDs red
    // Send buffer to Sense HAT
    fb.write_all(&buffer)?;
    fb.seek(SeekFrom::Start(0))?;

    // Keep the LEDs lit for 5 seconds
    thread::sleep(Duration::from_secs(3));

    // Turn off the LEDs by sending a buffer of zeros
    fb.write_all(&[0; 128])?;
    fb.seek(SeekFrom::Start(0))?;

    // Get the interface to capture packets from
    let interface_name = env::args().nth(1).expect("Usage: cargo run <interface_name>");

    // Find the network interface by name
    let interfaces = datalink::interfaces();
    let interface = interfaces
        .into_iter()
        .find(|iface| iface.name == interface_name)
        .expect("Interface not found");

    // Configure the interface for promiscuous mode
    let mut config = Config::default();
    config.promiscuous = true;

    // Create a datalink channel to capture packets
    let mut channel = match datalink::channel(&interface, config) {
        Ok(Channel::Ethernet(_rx, tx)) => tx,
        Ok(_) => panic!("Unhandled channel type"),
        Err(e) => panic!("Failed to create datalink channel: {}", e),
    };

    println!("Listening on interface: {}", interface_name);

    // Input packet size and compressed output size
    let input_size = 1518; // Max Ethernet packet size
    let compressed_size = 32; // Compressed size for Sense HAT

    // Create the compressor network
    let compressor = PacketCompressor::new(input_size, compressed_size);

    // Capture and process packets
    loop {
        match channel.next() {
            Ok(packet) => {
                let compressed = compressor.compress(bytes_to_f32_vector(packet));

                let buffer: &[u8] = unsafe {
                    std::slice::from_raw_parts(
                        compressed.as_ptr() as *const u8,
                        compressed.len() * std::mem::size_of::<f32>(),
                    )
                };
                fb.write_all(&buffer)?;
                fb.seek(SeekFrom::Start(0))?;
            }
            Err(e) => {
                eprintln!("Failed to read packet: {}", e);
            }
        }
    }

    Ok(())
}

