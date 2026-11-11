//! Poll loops must time out on a bus whose transfers finish without suspending.

extern crate std;

use core::convert::Infallible;
use core::task::{Context, Poll};
use std::collections::VecDeque;

use aligned::{Aligned, A4};
use embassy_futures::block_on;
use embedded_hal_async::spi::{ErrorType, Operation, SpiDevice};

use super::*;

/// Answers each byte read with the next scripted byte, then with `fill`. Writes are ignored.
struct Bus {
    script: VecDeque<u8>,
    fill: u8,
}

impl ErrorType for Bus {
    type Error = Infallible;
}

impl SpiDevice for Bus {
    async fn transaction(
        &mut self,
        operations: &mut [Operation<'_, u8>],
    ) -> Result<(), Infallible> {
        for operation in operations {
            if let Operation::Read(words) | Operation::TransferInPlace(words) = operation {
                for word in words.iter_mut() {
                    *word = self.script.pop_front().unwrap_or(self.fill);
                }
            }
        }
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
    };
    let mut driver = SdSpi::new(bus, PollDelay);
    driver.card = Some(Card::default());
    driver
}

fn read(driver: &mut SdSpi<Bus, PollDelay, A4>) -> Result<(), Error> {
    let mut block = [Aligned([0u8; 512])];
    block_on(driver.read(0, &mut block))
}

#[test]
fn stuck_busy_times_out() {
    assert_eq!(read(&mut driver(&[], 0x00)), Err(Error::Timeout));
}

#[test]
fn removed_card_times_out_waiting_for_r1() {
    assert_eq!(read(&mut driver(&[], 0xFF)), Err(Error::Timeout));
}

#[test]
fn removed_card_times_out_waiting_for_read_token() {
    // Idle before CMD17, R1 ready, then 0xFF forever.
    assert_eq!(read(&mut driver(&[0xFF, 0x00], 0xFF)), Err(Error::Timeout));
}
