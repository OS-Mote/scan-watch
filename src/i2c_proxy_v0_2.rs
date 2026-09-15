use core::cell::RefCell;
use embedded_hal_compat::ReverseCompat;
pub struct I2cProxyV0_2(pub &'static RefCell<esp_hal::i2c::master::I2c<'static, esp_hal::Blocking>>);

impl embedded_hal_02::blocking::i2c::WriteRead for I2cProxyV0_2 {
    type Error = esp_hal::i2c::master::Error;

    fn write_read(
        &mut self,
        address: u8,
        write: &[u8],
        read: &mut [u8],
    ) -> Result<(), Self::Error> {
        // Briefly borrow the I2C bus inside this operation scope
        let mut guard = self.0.borrow_mut();
        let mut v0_2_adapter = (&mut *guard).reverse();
        v0_2_adapter.write_read(address, write, read)
    }
}

impl embedded_hal_02::blocking::i2c::Write for I2cProxyV0_2 {
    type Error = esp_hal::i2c::master::Error;

    fn write(&mut self, address: u8, write: &[u8]) -> Result<(), Self::Error> {
        let mut guard = self.0.borrow_mut();
        let mut v0_2_adapter = (&mut *guard).reverse();
        v0_2_adapter.write(address, write)
    }
}