use rppal::i2c::I2c;
use std::thread;
use std::time::Duration;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Initialize I2C interface
    let mut i2c = I2c::new()?;
    
    // Set the address for the Sense HAT LED matrix
    i2c.set_slave_address(0x46)?;

    // Define a simple RGB pattern for the 8x8 LED matrix
    let mut buffer = [0u8; 192]; // 8x8 RGB matrix (8 rows x 8 columns x 3 bytes per LED)

    // Fill buffer with colors (red, green, blue)
    for i in 0..64 {
        buffer[i * 3] = 255; // Red
        buffer[i * 3 + 1] = 0; // Green
        buffer[i * 3 + 2] = 0; // Blue
    }

    // Send buffer to Sense HAT
    i2c.block_write(0x00, &buffer)?;

    // Keep the LEDs lit for 5 seconds
    thread::sleep(Duration::from_secs(5));

    // Turn off the LEDs by sending a buffer of zeros
    i2c.block_write(0x00, &[0; 192])?;

    Ok(())
}

