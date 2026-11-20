//! A crate for interfacing with SD cards over SPI.

#![no_std]

use aligned::Aligned;
use core::fmt::Debug;
use core::future::Future;
use core::marker::PhantomData;
use embassy_futures::select::{select, Either};
use embassy_futures::yield_now;
use embedded_hal_async::spi::Operation;
use sdio_host::sd::{CardCapacity, CID, CSD, OCR, SD};
use sdio_host::{common_cmd::*, sd_cmd::*};

// MUST be the first module listed
mod fmt;

#[cfg(test)]
mod tests;

/// Status for card in the ready state
pub const R1_READY_STATE: u8 = 0x00;
/// Status for card in the idle state
pub const R1_IDLE_STATE: u8 = 0x01;
/// Status bit for illegal command
pub const R1_ILLEGAL_COMMAND: u8 = 0x04;
/// Start data token for read or write single block*/
pub const DATA_START_BLOCK: u8 = 0xFE;
/// Stop token for write multiple blocks*/
pub const STOP_TRAN_TOKEN: u8 = 0xFD;
/// Start data token for write multiple blocks*/
pub const WRITE_MULTIPLE_TOKEN: u8 = 0xFC;
/// Mask for data response tokens after a write block operation
pub const DATA_RES_MASK: u8 = 0x1F;
/// Write data accepted token
pub const DATA_RES_ACCEPTED: u8 = 0x05;

#[derive(Clone, Copy, Debug, Default)]
/// SD Card
pub struct Card {
    /// The type of this card
    pub card_type: CardCapacity,
    /// Operation Conditions Register
    pub ocr: OCR<SD>,
    /// Relative Card Address
    pub rca: u32,
    /// Card ID
    pub cid: CID<SD>,
    /// Card Specific Data
    pub csd: CSD<SD>,
}

impl Card {
    /// Size in bytes
    pub fn size(&self) -> u64 {
        // SDHC / SDXC / SDUC
        u64::from(self.csd.block_count()) * 512
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
#[non_exhaustive]
pub enum Error {
    ChipSelect,
    SpiError,
    Timeout,
    UnsupportedCard,
    Cmd58Error,
    Cmd59Error,
    RegisterError(u8),
    CrcMismatch(u16, u16),
    NotInitialized,
    WriteError,
}

/// Must be called between powerup and [SdSpi::init] to ensure the sdcard is properly initialized.
pub async fn sd_init<SPI, CS, BE>(spi: &mut SPI, cs: &mut CS) -> Result<(), Error>
where
    SPI: embedded_hal_async::spi::SpiBus<Error = BE>,
    CS: embedded_hal::digital::OutputPin,
{
    // Supply minimum of 74 clock cycles without CS asserted.
    cs.set_high().map_err(|_| Error::ChipSelect)?;
    spi.write(&[0xFF; 10]).await.map_err(|_| Error::SpiError)?;

    Ok(())
}

pub struct SdSpi<SPI, D, ALIGN>
where
    SPI: embedded_hal_async::spi::SpiDevice,
    D: embedded_hal_async::delay::DelayNs,
    ALIGN: aligned::Alignment,
{
    spi: SPI,
    delay: D,
    card: Option<Card>,
    _align: PhantomData<ALIGN>,
}

impl<SPI, D, ALIGN> SdSpi<SPI, D, ALIGN>
where
    SPI: embedded_hal_async::spi::SpiDevice,
    D: embedded_hal_async::delay::DelayNs + Clone,
    ALIGN: aligned::Alignment,
{
    pub fn new(spi: SPI, delay: D) -> Self {
        Self {
            spi,
            delay,
            card: None,
            _align: PhantomData,
        }
    }

    /// To comply with the SD card spec, [sd_init] must be called between powerup and calling this function.
    pub async fn init(&mut self) -> Result<(), Error> {
        // A failed init must not leave a previous card's state in place.
        self.card = None;
        let r = async {
            with_stage_timeout(self.delay.clone(), 1000, "CMD0 idle", async {
                loop {
                    match self.cmd(idle()).await {
                        Ok(R1_IDLE_STATE) => return Ok(()),
                        Ok(_) | Err(Error::Timeout) => {}
                        Err(e) => return Err(e),
                    }
                    self.delay.delay_ms(1).await;
                }
            })
            .await?;

            with_stage_timeout(self.delay.clone(), 1000, "CMD8", async {
                loop {
                    let mut buffer = [0xFFu8; 4];
                    let r = self
                        .cmd_with_data(send_if_cond(0x1, 0xAA), &mut buffer)
                        .await?;
                    if r == (R1_ILLEGAL_COMMAND | R1_IDLE_STATE) {
                        return Err(Error::UnsupportedCard);
                    }
                    if r != R1_IDLE_STATE {
                        return Err(Error::RegisterError(r));
                    }
                    if buffer[3] == 0xAA {
                        // SD spec §4.3.13 / §7.3.1.4: verify voltage accepted (0x01 = 2.7-3.6V).
                        if (buffer[2] & 0x0F) != 0x01 {
                            return Err(Error::UnsupportedCard);
                        }
                        return Ok(());
                    }
                    self.delay.delay_ms(5).await;
                }
            })
            .await?;

            // "The SPI interface is initialized in the CRC OFF mode in default"
            // -- SD Part 1 Physical Layer Specification v9.00, Section 7.2.2 Bus Transfer Protection
            if self.cmd(cmd::<R1>(0x3B, 1)).await? != R1_IDLE_STATE {
                return Err(Error::Cmd59Error);
            }

            trace!("Valid card detected!");

            // If we get here we're at least a v2 card
            let mut card = Card::default();

            // Send ACMD41 with HCS=1 and reserved/S18R bits cleared (0x4000_0000 per SPI spec §7.3.1.3).
            with_stage_timeout(self.delay.clone(), 2000, "ACMD41 ready", async {
                loop {
                    let r = self
                        .acmd(sd_send_op_cond(true, false, false, 0))
                        .await?;
                    if r == R1_READY_STATE {
                        return Ok(());
                    }
                    if (r & !R1_IDLE_STATE) != 0 {
                        return Err(Error::RegisterError(r));
                    }
                    self.delay.delay_ms(5).await;
                }
            })
            .await?;

            trace!("send_ocr");
            card.ocr = with_stage_timeout(self.delay.clone(), 1000, "CMD58 OCR", async {
                loop {
                    let mut buffer = [0xFFu8; 4];
                    let r = self
                        .cmd_with_data(cmd::<R3>(0x3A, 0), &mut buffer)
                        .await?;
                    if r != R1_READY_STATE {
                        return Err(Error::Cmd58Error);
                    }
                    let ocr: OCR<SD> = u32::from_be_bytes(buffer).into();
                    if !ocr.is_busy() {
                        return Ok(ocr);
                    }
                    self.delay.delay_ms(5).await;
                }
            })
            .await?;

            // Reads and writes send block numbers, which only SDHC/SDXC cards accept. An SDSC card
            // expects byte addresses and would be written at the wrong place, so refuse it.
            if !card.ocr.high_capacity() {
                return Err(Error::UnsupportedCard);
            }
            card.card_type = CardCapacity::HighCapacity;

            trace!("send_csd");
            let mut csd = [0xFFu8; 16];
            self.cmd_read_data(send_csd(card.rca as u16), &mut csd)
                .await?;
            card.csd = u128::from_be_bytes(csd).into();

            trace!("all_send_cid");
            let mut cid = [0xFFu8; 16];
            self.cmd_read_data(send_cid(card.rca as u16), &mut cid)
                .await?;
            card.cid = u128::from_be_bytes(cid).into();

            trace!("Card initialized: {:?}", card);
            debug!("Found card with size: {}bytes", card.size());

            self.card = Some(card);

            Ok(())
        }
        .await;

        r
    }

    pub async fn read<const SIZE: usize>(
        &mut self,
        block_address: u32,
        data: &mut [Aligned<ALIGN, [u8; SIZE]>],
    ) -> Result<(), Error> {
        if self.card.is_none() {
            return Err(Error::NotInitialized);
        }
        if data.is_empty() {
            return Ok(());
        }
        let r = async {
            if data.len() == 1 {
                self.cmd_read_data(read_single_block(block_address), &mut data[0][..])
                    .await?;
            } else {
                let mut tail = [0xFFu8; 16];
                let (r1, tail_len) = self
                    .cmd_with_tail(read_multiple_blocks(block_address), &mut tail)
                    .await?;
                check_r1(r1)?;

                let mut result = self
                    .read_data_from_tail(&tail[..tail_len], &mut data[0][..])
                    .await;
                if result.is_ok() {
                    for block in &mut data[1..] {
                        result = self.read_data(&mut block[..]).await;
                        if result.is_err() {
                            break;
                        }
                    }
                }
                // CMD12 also ends the transfer after a failed block, so the card takes the next
                // command instead of still sending data.
                let stop = self.cmd(stop_transmission()).await;
                result?;
                stop?;
            }
            Ok(())
        }
        .await;

        r?;

        Ok(())
    }

    pub async fn write<const SIZE: usize>(
        &mut self,
        block_address: u32,
        data: &[Aligned<ALIGN, [u8; SIZE]>],
    ) -> Result<(), Error> {
        if self.card.is_none() {
            return Err(Error::NotInitialized);
        }
        if data.is_empty() {
            return Ok(());
        }
        let r = async {
            if data.len() == 1 {
                check_r1(self.cmd(write_single_block(block_address)).await?)?;
                self.write_data(DATA_START_BLOCK, &data[0][..]).await?;
                self.wait_idle().await?;
                // Check status: in SD SPI mode, CMD13 returns a 2-byte R2 response.
                let mut status2 = [0xFFu8; 1];
                let r1 = self.cmd_with_data(sd_status(), &mut status2).await?;
                if r1 != 0 || status2[0] != 0 {
                    return Err(Error::WriteError);
                }
            } else {
                // Try sending ACMD23 _before_ write.
                // This will pre-erase blocks to improve write performance.
                // We ignore the return value, because whether its accepted
                // or not doesn't matter we will still proceed with the write
                self.acmd(cmd::<R1>(0x17, data.len() as u32)).await?;
                self.wait_idle().await?;

                check_r1(self.cmd(write_multiple_blocks(block_address)).await?)?;
                let mut result = Ok(());
                for block in data {
                    result = async {
                        self.wait_idle().await?;
                        self.write_data(WRITE_MULTIPLE_TOKEN, &block[..]).await
                    }
                    .await;
                    if result.is_err() {
                        break;
                    }
                }
                // The stop token also ends the transfer after a failed block, so the card leaves
                // the receive-data state.
                let stop = async {
                    self.wait_idle().await?;
                    // Clock the mandatory 1-byte Nbr stuff byte in the same CS-low transaction so
                    // the busy wait afterwards does not read it as idle 0xFF.
                    let mut stuff = [0xFFu8; 1];
                    self.spi
                        .transaction(&mut [
                            Operation::Write(&[STOP_TRAN_TOKEN]),
                            Operation::TransferInPlace(&mut stuff),
                        ])
                        .await
                        .map_err(|_| Error::SpiError)?;
                    self.wait_idle().await
                }
                .await;
                result?;
                stop?;
            }
            Ok(())
        }
        .await;

        r?;

        Ok(())
    }

    pub async fn size(&mut self) -> Result<u64, Error> {
        Ok(self.card.ok_or(Error::NotInitialized)?.size())
    }

    async fn read_data(&mut self, buffer: &mut [u8]) -> Result<(), Error> {
        let r = with_timeout(self.delay.clone(), 1000, self.poll_byte(|b| b != 0xFF)).await??;

        if r != DATA_START_BLOCK {
            return Err(Error::RegisterError(r));
        }

        self.finish_read_data(&[], buffer).await
    }

    async fn finish_read_data(&mut self, prefix: &[u8], buffer: &mut [u8]) -> Result<(), Error> {
        let data_copied = prefix.len().min(buffer.len());
        buffer[..data_copied].copy_from_slice(&prefix[..data_copied]);

        let rem_prefix = &prefix[data_copied..];
        let mut crc_bytes = [0xFFu8; 2];
        let crc_copied = rem_prefix.len().min(2);
        crc_bytes[..crc_copied].copy_from_slice(&rem_prefix[..crc_copied]);

        if data_copied < buffer.len() {
            buffer[data_copied..].fill(0xFF);
            self.spi
                .transaction(&mut [
                    Operation::TransferInPlace(&mut buffer[data_copied..]),
                    Operation::TransferInPlace(&mut crc_bytes[crc_copied..]),
                ])
                .await
                .map_err(|_| Error::SpiError)?;
        } else if crc_copied < 2 {
            self.spi
                .transfer_in_place(&mut crc_bytes[crc_copied..])
                .await
                .map_err(|_| Error::SpiError)?;
        }

        let crc = u16::from_be_bytes(crc_bytes);
        let calc_crc = crc16(buffer);
        if crc != calc_crc {
            return Err(Error::CrcMismatch(crc, calc_crc));
        }

        Ok(())
    }

    async fn read_data_from_tail(&mut self, tail: &[u8], buffer: &mut [u8]) -> Result<(), Error> {
        if let Some(tok_pos) = tail.iter().position(|&b| b != 0xFF) {
            let token = tail[tok_pos];
            if token != DATA_START_BLOCK {
                return Err(Error::RegisterError(token));
            }
            return self.finish_read_data(&tail[tok_pos + 1..], buffer).await;
        }

        self.read_data(buffer).await
    }

    async fn cmd_read_data<R: Resp>(&mut self, cmd: Cmd<R>, buffer: &mut [u8]) -> Result<(), Error> {
        let mut tail = [0xFFu8; 16];
        let (r1, tail_len) = self.cmd_with_tail(cmd, &mut tail).await?;
        check_r1(r1)?;

        self.read_data_from_tail(&tail[..tail_len], buffer).await
    }

    async fn write_data(&mut self, token: u8, buffer: &[u8]) -> Result<(), Error> {
        let crc_bytes = crc16(buffer).to_be_bytes();
        let mut status = [0xFFu8; 1];
        // Prepend a 0xFF byte to guarantee Nwr >= 1 byte between command R1 and data start token.
        self.spi
            .transaction(&mut [
                Operation::Write(&[0xFF, token]),
                Operation::Write(buffer),
                Operation::Write(&crc_bytes),
                Operation::TransferInPlace(&mut status),
            ])
            .await
            .map_err(|_| Error::SpiError)?;

        if (status[0] & DATA_RES_MASK) != DATA_RES_ACCEPTED {
            return Err(Error::WriteError);
        }

        Ok(())
    }

    pub fn spi(&mut self) -> &mut SPI {
        &mut self.spi
    }

    async fn cmd<R: Resp>(&mut self, cmd: Cmd<R>) -> Result<u8, Error> {
        self.cmd_with_data(cmd, &mut []).await
    }

    async fn cmd_with_data<R: Resp>(&mut self, cmd: Cmd<R>, extra: &mut [u8]) -> Result<u8, Error> {
        let (r1, copied) = self.cmd_with_tail(cmd, extra).await?;
        if copied < extra.len() {
            extra[copied..].fill(0xFF);
            self.spi
                .transfer_in_place(&mut extra[copied..])
                .await
                .map_err(|_| Error::SpiError)?;
        }
        Ok(r1)
    }

    /// Sends `cmd` with CS held low through R1, copying any trailing bytes already clocked into
    /// `tail` and returning `(r1, bytes_copied)`.
    async fn cmd_with_tail<R: Resp>(
        &mut self,
        cmd: Cmd<R>,
        tail: &mut [u8],
    ) -> Result<(u8, usize), Error> {
        if cmd.cmd != idle().cmd {
            self.wait_idle().await?;
        }

        let mut buf = [
            0x40 | cmd.cmd,
            (cmd.arg >> 24) as u8,
            (cmd.arg >> 16) as u8,
            (cmd.arg >> 8) as u8,
            cmd.arg as u8,
            0,
        ];
        buf[5] = crc7(&buf[0..5]);

        // Keep CS asserted across the command and its Ncr + R1 (+ optional trailing) bytes.
        let mut rx = [0xFFu8; 16];
        let rx_len = (10 + tail.len()).min(rx.len());
        if cmd.cmd == stop_transmission().cmd {
            let mut stuff = [0xFFu8; 1];
            self.spi
                .transaction(&mut [
                    Operation::Write(&buf),
                    Operation::TransferInPlace(&mut stuff),
                    Operation::TransferInPlace(&mut rx[..rx_len]),
                ])
                .await
                .map_err(|_| Error::SpiError)?;
        } else {
            self.spi
                .transaction(&mut [
                    Operation::Write(&buf),
                    Operation::TransferInPlace(&mut rx[..rx_len]),
                ])
                .await
                .map_err(|_| Error::SpiError)?;
        }

        if let Some(pos) = rx[..rx_len].iter().position(|&b| b & 0x80 == 0) {
            let r1 = rx[pos];
            let avail = (rx_len - (pos + 1)).min(tail.len());
            if avail > 0 {
                tail[..avail].copy_from_slice(&rx[pos + 1..pos + 1 + avail]);
            }
            return Ok((r1, avail));
        }

        Err(Error::Timeout)
    }

    async fn acmd<R: Resp>(&mut self, cmd: Cmd<R>) -> Result<u8, Error> {
        let r1 = self
            .cmd(app_cmd(self.card.map(|c| c.rca).unwrap_or(0) as u16))
            .await?;
        if (r1 & !R1_IDLE_STATE) != 0 {
            return Err(Error::RegisterError(r1));
        }
        self.cmd(cmd).await
    }

    /// Waits until the card stops signalling busy. After 16 polls it sleeps 100 µs before each
    /// further poll. This is the driver's only `delay_us` call, so a delay implementation can time
    /// it as card busy time.
    async fn wait_idle(&mut self) -> Result<(), Error> {
        let mut sleep = self.delay.clone();
        with_timeout(self.delay.clone(), 5000, async {
            let mut polls = 0;
            while self.read_byte().await? != 0xFF {
                polls += 1;
                if polls > 16 {
                    sleep.delay_us(100).await;
                }
            }
            Ok(())
        })
        .await?
    }

    /// Reads bytes until `done` accepts one. It yields every 8 bytes, so the caller's timeout can
    /// run on a bus that finishes transfers without suspending.
    async fn poll_byte(&mut self, done: impl Fn(u8) -> bool) -> Result<u8, Error> {
        let mut polls = 0u32;
        loop {
            let byte = self.read_byte().await?;
            if done(byte) {
                return Ok(byte);
            }
            polls += 1;
            if polls.is_multiple_of(8) {
                yield_now().await;
            }
        }
    }

    async fn read_byte(&mut self) -> Result<u8, Error> {
        let mut buf = [0xFFu8; 1];
        self.spi
            .transfer_in_place(&mut buf[..])
            .await
            .map_err(|_| Error::SpiError)?;

        Ok(buf[0])
    }
}

impl<SPI, D, ALIGN, const SIZE: usize> block_device_driver::BlockDevice<SIZE>
    for SdSpi<SPI, D, ALIGN>
where
    SPI: embedded_hal_async::spi::SpiDevice,
    D: embedded_hal_async::delay::DelayNs + Clone,
    ALIGN: aligned::Alignment,
{
    type Error = Error;
    type Align = ALIGN;

    async fn read(
        &mut self,
        block_address: u32,
        data: &mut [Aligned<ALIGN, [u8; SIZE]>],
    ) -> Result<(), Self::Error> {
        self.read(block_address, data).await
    }

    async fn write(
        &mut self,
        block_address: u32,
        data: &[Aligned<ALIGN, [u8; SIZE]>],
    ) -> Result<(), Self::Error> {
        self.write(block_address, data).await
    }

    async fn size(&mut self) -> Result<u64, Self::Error> {
        self.size().await
    }
}

async fn with_timeout<D: embedded_hal_async::delay::DelayNs, F: Future>(
    mut delay: D,
    timeout: u32,
    fut: F,
) -> Result<F::Output, Error> {
    match select(fut, delay.delay_ms(timeout)).await {
        Either::First(r) => Ok(r),
        Either::Second(_) => Err(Error::Timeout),
    }
}

async fn with_stage_timeout<D, F, T>(
    delay: D,
    timeout: u32,
    stage: &str,
    fut: F,
) -> Result<T, Error>
where
    D: embedded_hal_async::delay::DelayNs,
    F: Future<Output = Result<T, Error>>,
{
    match with_timeout(delay, timeout, fut).await {
        Ok(Ok(v)) => Ok(v),
        Ok(Err(Error::Timeout)) | Err(Error::Timeout) => {
            warn!("SD init timeout waiting for {}", stage);
            Err(Error::Timeout)
        }
        Ok(Err(e)) | Err(e) => Err(e),
    }
}

/// Fails unless a command's R1 response reports no error. A data command the card rejected must
/// not be followed by a data phase.
fn check_r1(r1: u8) -> Result<(), Error> {
    if r1 == R1_READY_STATE {
        Ok(())
    } else {
        Err(Error::RegisterError(r1))
    }
}

/// Perform the 7-bit CRC used on the SD card
fn crc7(data: &[u8]) -> u8 {
    let mut crc = 0u8;
    for mut d in data.iter().cloned() {
        for _bit in 0..8 {
            crc <<= 1;
            if ((d & 0x80) ^ (crc & 0x80)) != 0 {
                crc ^= 0x09;
            }
            d <<= 1;
        }
    }
    (crc << 1) | 1
}

/// Perform the X25 CRC calculation, as used for data blocks.
fn crc16(data: &[u8]) -> u16 {
    let mut crc = 0u16;
    for &byte in data {
        crc = ((crc >> 8) & 0xFF) | (crc << 8);
        crc ^= u16::from(byte);
        crc ^= (crc & 0xFF) >> 4;
        crc ^= crc << 12;
        crc ^= (crc & 0xFF) << 5;
    }
    crc
}
