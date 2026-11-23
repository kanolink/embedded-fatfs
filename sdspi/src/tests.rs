//! Poll loops must time out on a bus whose transfers finish without suspending.

extern crate std;

use core::convert::Infallible;
use core::task::{Context, Poll};
use std::collections::VecDeque;

use std::vec;
use std::vec::Vec;

use aligned::{Aligned, A4};
use embassy_futures::block_on;
use embedded_hal_async::spi::{ErrorType, Operation, SpiDevice};

use super::*;

#[derive(Clone, Debug, PartialEq, Eq)]
struct RecordedTx {
    writes: Vec<u8>,
    read_lens: Vec<usize>,
}

/// Answers each byte read with the next scripted byte, then with `fill`.
/// Also records every SPI transaction boundary and written bytes.
struct Bus {
    script: VecDeque<u8>,
    fill: u8,
    txs: Vec<RecordedTx>,
}

impl ErrorType for Bus {
    type Error = Infallible;
}

impl SpiDevice for Bus {
    async fn transaction(
        &mut self,
        operations: &mut [Operation<'_, u8>],
    ) -> Result<(), Infallible> {
        let mut writes = Vec::new();
        let mut read_lens = Vec::new();
        for operation in operations {
            match operation {
                Operation::Write(words) => {
                    writes.extend_from_slice(words);
                }
                Operation::Read(words) => {
                    read_lens.push(words.len());
                    for word in words.iter_mut() {
                        *word = self.script.pop_front().unwrap_or(self.fill);
                    }
                }
                Operation::TransferInPlace(words) => {
                    read_lens.push(words.len());
                    for word in words.iter_mut() {
                        *word = self.script.pop_front().unwrap_or(self.fill);
                    }
                }
                Operation::Transfer(read, write) => {
                    writes.extend_from_slice(write);
                    read_lens.push(read.len());
                    for word in read.iter_mut() {
                        *word = self.script.pop_front().unwrap_or(self.fill);
                    }
                }
                Operation::DelayNs(_) => {}
            }
        }
        self.txs.push(RecordedTx { writes, read_lens });
        Ok(())
    }
}

/// A delay that ends after 50 polls.
#[derive(Clone)]
struct PollDelay;

impl embedded_hal_async::delay::DelayNs for PollDelay {
    async fn delay_ns(&mut self, _ns: u32) {
        let mut polls = 50;
        core::future::poll_fn(|cx: &mut Context<'_>| {
            if polls == 0 {
                return Poll::Ready(());
            }
            polls -= 1;
            cx.waker().wake_by_ref();
            Poll::Pending
        })
        .await
    }
}

fn driver(script: &[u8], fill: u8) -> SdSpi<Bus, PollDelay, A4> {
    let bus = Bus {
        script: script.iter().copied().collect(),
        fill,
        txs: Vec::new(),
    };
    let mut driver = SdSpi::new(bus, PollDelay);
    driver.card = Some(Card::default());
    driver
}

fn uninit_driver(script: &[u8], fill: u8) -> SdSpi<Bus, PollDelay, A4> {
    let bus = Bus {
        script: script.iter().copied().collect(),
        fill,
        txs: Vec::new(),
    };
    SdSpi::new(bus, PollDelay)
}

fn read(driver: &mut SdSpi<Bus, PollDelay, A4>) -> Result<(), Error> {
    let mut block = [Aligned([0u8; 512])];
    block_on(driver.read(0, &mut block))
}

fn data_packet(payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(1 + payload.len() + 2);
    out.push(DATA_START_BLOCK);
    out.extend_from_slice(payload);
    out.extend_from_slice(&crc16(payload).to_be_bytes());
    out
}

/// Appends a command response for `cmd_with_tail` (which clocks `rx_len = (10 + tail_len).min(16)` bytes).
fn push_cmd_resp(script: &mut Vec<u8>, wait_idle: bool, ncr: usize, r1: u8, extra: &[u8], rx_len: usize) {
    if wait_idle {
        script.push(0xFF);
    }
    script.extend(core::iter::repeat_n(0xFF, ncr));
    script.push(r1);
    script.extend_from_slice(extra);
    let clocked = ncr + 1 + extra.len();
    if clocked < rx_len {
        script.extend(core::iter::repeat_n(0xFF, rx_len - clocked));
    }
}

#[test]
fn stuck_busy_times_out() {
    assert_eq!(read(&mut driver(&[], 0x00)), Err(Error::BusyTimeout));
}

#[test]
fn removed_card_times_out_waiting_for_r1() {
    assert_eq!(read(&mut driver(&[], 0xFF)), Err(Error::NoResponse));
}

#[test]
fn removed_card_times_out_waiting_for_read_token() {
    // Idle before CMD17, R1 ready, then 0xFF forever.
    assert_eq!(read(&mut driver(&[0xFF, 0x00], 0xFF)), Err(Error::ReadTimeout));
}

#[test]
fn init_happy_path_and_cs_transactions() {
    let mut script = Vec::new();
    // CMD0 (no wait_idle): 1st attempt misses R1 (10x 0xFF), 2nd attempt succeeds with R1_IDLE_STATE.
    script.extend(core::iter::repeat_n(0xFF, 10));
    push_cmd_resp(&mut script, false, 1, R1_IDLE_STATE, &[], 10);
    // CMD8: R1_IDLE_STATE + R7 [0x00, 0x00, 0x01, 0xAA]
    push_cmd_resp(&mut script, true, 1, R1_IDLE_STATE, &[0x00, 0x00, 0x01, 0xAA], 14);
    // CMD59 (CRC ON): R1_IDLE_STATE
    push_cmd_resp(&mut script, true, 1, R1_IDLE_STATE, &[], 10);
    // CMD55: R1_IDLE_STATE
    push_cmd_resp(&mut script, true, 1, R1_IDLE_STATE, &[], 10);
    // ACMD41: R1_READY_STATE
    push_cmd_resp(&mut script, true, 1, R1_READY_STATE, &[], 10);
    // CMD58: R1_READY_STATE + OCR (not busy = bit 31, CCS = bit 30 => 0xC0_FF_80_00)
    push_cmd_resp(&mut script, true, 1, R1_READY_STATE, &[0xC0, 0xFF, 0x80, 0x00], 14);
    // CMD9 (CSD, 16 bytes): R1_READY_STATE + DATA_START_BLOCK + 16B + CRC16
    let csd = [0x40u8; 16];
    let csd_pkt = data_packet(&csd);
    push_cmd_resp(&mut script, true, 1, R1_READY_STATE, &csd_pkt[..14], 16);
    script.extend_from_slice(&csd_pkt[14..]);
    // CMD10 (CID, 16 bytes): R1_READY_STATE + DATA_START_BLOCK + 16B + CRC16
    let cid = [0x11u8; 16];
    let cid_pkt = data_packet(&cid);
    push_cmd_resp(&mut script, true, 1, R1_READY_STATE, &cid_pkt[..14], 16);
    script.extend_from_slice(&cid_pkt[14..]);

    let mut drv = uninit_driver(&script, 0xFF);
    block_on(drv.init()).unwrap();

    // Extract all 6-byte command frames sent in single-command transactions.
    let cmds: Vec<(u8, u32)> = drv
        .spi()
        .txs
        .iter()
        .filter(|tx| tx.writes.len() == 6 && (tx.writes[0] & 0xC0) == 0x40)
        .map(|tx| {
            let cmd = tx.writes[0] & 0x3F;
            let arg = u32::from_be_bytes([tx.writes[1], tx.writes[2], tx.writes[3], tx.writes[4]]);
            (cmd, arg)
        })
        .collect();

    assert_eq!(
        cmds,
        vec![
            (0, 0),            // CMD0 (missed)
            (0, 0),            // CMD0 (retry succeeded)
            (8, 0x1AA),        // CMD8
            (59, 1),           // CMD59 after CMD8
            (55, 0),           // CMD55
            (41, 0x4000_0000), // ACMD41 with HCS=1, S18R=0, reserved=0
            (58, 0),           // CMD58
            (9, 0),            // CMD9
            (10, 0),           // CMD10
        ]
    );
}

#[test]
fn cmd8_rejects_bad_voltage() {
    let mut script = Vec::new();
    push_cmd_resp(&mut script, false, 1, R1_IDLE_STATE, &[], 10);
    // R7 with voltage accepted = 0x00 (invalid) and check pattern = 0xAA
    push_cmd_resp(&mut script, true, 1, R1_IDLE_STATE, &[0x00, 0x00, 0x00, 0xAA], 14);

    let mut drv = uninit_driver(&script, 0xFF);
    assert_eq!(block_on(drv.init()), Err(Error::UnsupportedCard));
}

#[test]
fn acmd41_r1_error_fails_fast() {
    let mut script = Vec::new();
    push_cmd_resp(&mut script, false, 1, R1_IDLE_STATE, &[], 10);
    push_cmd_resp(&mut script, true, 1, R1_IDLE_STATE, &[0x00, 0x00, 0x01, 0xAA], 14);
    push_cmd_resp(&mut script, true, 1, R1_IDLE_STATE, &[], 10);
    // CMD55 returns illegal command
    push_cmd_resp(&mut script, true, 1, R1_IDLE_STATE | R1_ILLEGAL_COMMAND, &[], 10);

    let mut drv = uninit_driver(&script, 0xFF);
    assert_eq!(
        block_on(drv.init()),
        Err(Error::RegisterError(R1_IDLE_STATE | R1_ILLEGAL_COMMAND))
    );
}

#[test]
fn silent_card_init_reports_no_response() {
    let mut drv = uninit_driver(&[], 0xFF);
    assert_eq!(block_on(drv.init()), Err(Error::NoResponse));
}

#[test]
fn acmd41_stuck_idle_reports_busy_timeout() {
    let mut script = Vec::new();
    push_cmd_resp(&mut script, false, 1, R1_IDLE_STATE, &[], 10);
    push_cmd_resp(&mut script, true, 1, R1_IDLE_STATE, &[0x00, 0x00, 0x01, 0xAA], 14);
    push_cmd_resp(&mut script, true, 1, R1_IDLE_STATE, &[], 10);
    // CMD55 + ACMD41 keep answering "still initializing" until the stage limit runs out.
    for _ in 0..16 {
        push_cmd_resp(&mut script, true, 1, R1_IDLE_STATE, &[], 10);
        push_cmd_resp(&mut script, true, 1, R1_IDLE_STATE, &[], 10);
    }

    let mut drv = uninit_driver(&script, 0xFF);
    assert_eq!(block_on(drv.init()), Err(Error::BusyTimeout));
}

#[test]
fn single_block_write_checks_cmd13_r2() {
    let block = [Aligned([0xA5u8; 512])];

    // Success case: CMD24 -> write_data (accepted 0x05) -> wait_idle -> CMD13 R2 [0x00, 0x00]
    let mut script = Vec::new();
    push_cmd_resp(&mut script, true, 1, R1_READY_STATE, &[], 10);
    script.push(DATA_RES_ACCEPTED);
    script.push(0xFF); // wait_idle
    push_cmd_resp(&mut script, true, 1, R1_READY_STATE, &[0x00], 11);

    let mut drv = driver(&script, 0xFF);
    block_on(drv.write(42, &block)).unwrap();

    // Verify write_data transaction starts with [0xFF, DATA_START_BLOCK] (Nwr >= 1).
    let write_tx = drv
        .spi()
        .txs
        .iter()
        .find(|tx| tx.writes.len() == 2 + 512 + 2)
        .expect("write_data transaction");
    assert_eq!(&write_tx.writes[..2], &[0xFF, DATA_START_BLOCK]);

    // Failure case: CMD13 2nd R2 byte is non-zero
    let mut fail_script = Vec::new();
    push_cmd_resp(&mut fail_script, true, 1, R1_READY_STATE, &[], 10);
    fail_script.push(DATA_RES_ACCEPTED);
    fail_script.push(0xFF);
    push_cmd_resp(&mut fail_script, true, 1, R1_READY_STATE, &[0x04], 11);

    let mut fail_drv = driver(&fail_script, 0xFF);
    assert_eq!(block_on(fail_drv.write(42, &block)), Err(Error::WriteError));
}

#[test]
fn multi_block_write_and_empty_write() {
    let mut empty_drv = driver(&[], 0xFF);
    let empty: [Aligned<A4, [u8; 512]>; 0] = [];
    block_on(empty_drv.write(0, &empty)).unwrap();
    assert!(empty_drv.spi().txs.is_empty());

    let blocks = [Aligned([0x11u8; 512]), Aligned([0x22u8; 512])];
    let mut script = Vec::new();
    // ACMD23: CMD55 + CMD23
    push_cmd_resp(&mut script, true, 1, R1_READY_STATE, &[], 10);
    push_cmd_resp(&mut script, true, 1, R1_READY_STATE, &[], 10);
    script.push(0xFF); // wait_idle
    // CMD25
    push_cmd_resp(&mut script, true, 1, R1_READY_STATE, &[], 10);
    // Block 0: wait_idle + write_data
    script.push(0xFF);
    script.push(DATA_RES_ACCEPTED);
    // Block 1: wait_idle + write_data
    script.push(0xFF);
    script.push(DATA_RES_ACCEPTED);
    // Stop: wait_idle + [STOP_TRAN_TOKEN] + stuff byte + wait_idle
    script.push(0xFF);
    script.push(0xFF); // stuff byte
    script.push(0xFF); // wait_idle

    let mut drv = driver(&script, 0xFF);
    block_on(drv.write(10, &blocks)).unwrap();

    // Verify STOP_TRAN_TOKEN + stuff byte are in a single transaction.
    let stop_tx = drv
        .spi()
        .txs
        .iter()
        .find(|tx| tx.writes == vec![STOP_TRAN_TOKEN])
        .expect("stop token tx");
    assert_eq!(stop_tx.read_lens, vec![1]);
}

#[test]
fn single_and_multi_block_read_paths() {
    // Empty read returns Ok(()) without SPI traffic.
    let mut empty_drv = driver(&[], 0xFF);
    let mut empty: [Aligned<A4, [u8; 512]>; 0] = [];
    block_on(empty_drv.read(0, &mut empty)).unwrap();
    assert!(empty_drv.spi().txs.is_empty());

    // Single-block read with 0xFE arriving inside CMD17 tail.
    let b0 = [0x33u8; 512];
    let pkt0 = data_packet(&b0);
    let mut s1 = Vec::new();
    push_cmd_resp(&mut s1, true, 1, R1_READY_STATE, &pkt0[..14], 16);
    s1.extend_from_slice(&pkt0[14..]);
    let mut d1 = driver(&s1, 0xFF);
    let mut out1 = [Aligned([0u8; 512])];
    block_on(d1.read(1, &mut out1)).unwrap();
    assert_eq!(*out1[0], b0);

    // Single-block read with CRC mismatch.
    let mut bad_pkt = pkt0.clone();
    let last = bad_pkt.len() - 1;
    bad_pkt[last] ^= 0xFF;
    let mut s_crc = Vec::new();
    push_cmd_resp(&mut s_crc, true, 1, R1_READY_STATE, &bad_pkt[..14], 16);
    s_crc.extend_from_slice(&bad_pkt[14..]);
    let mut d_crc = driver(&s_crc, 0xFF);
    assert!(matches!(
        block_on(d_crc.read(1, &mut out1)),
        Err(Error::CrcMismatch(_, _))
    ));

    // Multi-block read where block 0's 0xFE arrives immediately inside CMD18 tail,
    // and block 1 arrives via read_data.
    let b1 = [0x44u8; 512];
    let pkt1 = data_packet(&b1);
    let mut s2 = Vec::new();
    push_cmd_resp(&mut s2, true, 1, R1_READY_STATE, &pkt0[..14], 16);
    s2.extend_from_slice(&pkt0[14..]);
    s2.extend_from_slice(&pkt1);
    // CMD12 stop_transmission (wait_idle + 6B cmd + 1B stuff + rx)
    push_cmd_resp(&mut s2, true, 1, R1_READY_STATE, &[], 10);

    let mut d2 = driver(&s2, 0xFF);
    let mut out2 = [Aligned([0u8; 512]), Aligned([0u8; 512])];
    block_on(d2.read(2, &mut out2)).unwrap();
    assert_eq!(*out2[0], b0);
    assert_eq!(*out2[1], b1);

    // Multi-block read where block 0 arrives after >16 0xFF bytes.
    let mut s3 = Vec::new();
    push_cmd_resp(&mut s3, true, 1, R1_READY_STATE, &[], 16);
    s3.extend(core::iter::repeat_n(0xFF, 4));
    s3.extend_from_slice(&pkt0);
    s3.extend_from_slice(&pkt1);
    push_cmd_resp(&mut s3, true, 1, R1_READY_STATE, &[], 10);

    let mut d3 = driver(&s3, 0xFF);
    let mut out3 = [Aligned([0u8; 512]), Aligned([0u8; 512])];
    block_on(d3.read(3, &mut out3)).unwrap();
    assert_eq!(*out3[0], b0);
    assert_eq!(*out3[1], b1);

    // Multi-block read where block 0 returns a data error token still sends CMD12.
    let mut s4 = Vec::new();
    push_cmd_resp(&mut s4, true, 1, R1_READY_STATE, &[0x01], 16); // data error token 0x01
    push_cmd_resp(&mut s4, true, 1, R1_READY_STATE, &[], 10); // CMD12
    let mut d4 = driver(&s4, 0xFF);
    let mut out4 = [Aligned([0u8; 512]), Aligned([0u8; 512])];
    assert_eq!(block_on(d4.read(4, &mut out4)), Err(Error::RegisterError(0x01)));
    assert!(d4.spi().txs.iter().any(|tx| tx.writes.len() == 6 && (tx.writes[0] & 0x3F) == 12));
}

#[test]
fn cmd_read_data_small_buffer_uses_crc_from_tail() {
    let payload = [0xDE, 0xAD, 0xBE, 0xEF];
    let pkt = data_packet(&payload); // 1 + 4 + 2 = 7 bytes, all fit inside 14-byte tail!
    let mut script = Vec::new();
    push_cmd_resp(&mut script, true, 1, R1_READY_STATE, &pkt, 16);

    let mut drv = driver(&script, 0xFF);
    let mut buf = [0u8; 4];
    block_on(drv.cmd_read_data(send_csd(0), &mut buf)).unwrap();
    assert_eq!(buf, payload);
}
