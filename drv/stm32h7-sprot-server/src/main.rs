// This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

#![no_std]
#![no_main]

use core::convert::Into;
use drv_spi_api::{CsState, Spi};
use drv_sprot_api::*;
use drv_stm32xx_sys_api as sys_api;
use drv_update_api::{UpdateError, UpdateTarget};
use idol_runtime::{ClientError, Leased, RequestError, R, W};
use ringbuf::*;
use userlib::*;
#[cfg(feature = "sink_test")]
use zerocopy::{ByteOrder, LittleEndian};
// use serde::{Deserialize, Serialize};
// use hubpack::SerializedSize;

task_slot!(SPI, spi_driver);
task_slot!(SYS, sys);

#[allow(dead_code)]
#[derive(Copy, Clone, PartialEq)]
enum Trace {
    None,
    BadResponse(MsgType),
    BlockSize(usize),
    CSnAssert,
    CSnDeassert,
    Debug(bool),
    Error(SprotError),
    FailedRetries { retries: u16, errcode: SprotError },
    SprotError(SprotError),
    PulseFailed,
    RotNotReady,
    RotReadyTimeout,
    RxParseError(u8, u8, u8, u8),
    RxSpiError,
    RxPart1(usize),
    RxPart2(usize),
    SendRecv(usize),
    SinkFail(SprotError, u16),
    SinkLoop(u16),
    TxPart1(usize),
    TxPart2(usize),
    TxSize(usize),
    ErrRspPayloadSize(u16),
    UnexpectedRotIrq,
    UpdResponse(UpdateRspHeader),
    WrongMsgType(MsgType),
    UpdatePrep,
    UpdateWriteOneBlock,
    UpdateFinish,
    ErrRespNoPayload,
    Recoverable(SprotError),
    Header(MsgHeader),
    ErrWithHeader(SprotError, [u8; HEADER_SIZE]),
}
ringbuf!(Trace, 64, Trace::None);

const SP_TO_ROT_SPI_DEVICE: u8 = 0;

// TODO:These timeouts are somewhat arbitrary.
// TODO: Make timeouts configurable
// All timeouts are in 'ticks'

/// Retry timeout for send_recv_retries
const RETRY_TIMEOUT: u64 = 100;

/// Timeout for status message
const TIMEOUT_QUICK: u32 = 250;
/// Maximum timeout for an arbitrary message
const TIMEOUT_MAX: u32 = 500;
// XXX tune the RoT flash write timeout
const TIMEOUT_WRITE_ONE_BLOCK: u32 = 500;
// Delay between sending the portion of a message that fits entirely in the
// RoT's FIFO and the remainder of the message. This gives time for the RoT
// sprot task to respond to its interrupt.
const PART1_DELAY: u64 = 0;
const PART2_DELAY: u64 = 2; // Observed to be at least 2ms on gimletlet

const MAX_UPDATE_ATTEMPTS: u16 = 3;
cfg_if::cfg_if! {
    if #[cfg(feature = "sink_test")] {
        const MAX_SINKREQ_ATTEMPTS: u16 = 3; // TODO parameterize
    }
}

// ROT_IRQ comes from app.toml
// We use spi3 on gimletlet and spi4 on gemini and gimlet.
// You should be able to move the RoT board between SPI3, SPI4, and SPI6
// without much trouble even though SPI3 is the preferred connector and
// SPI4 is connected to the NET board.
cfg_if::cfg_if! {
    if #[cfg(any(
            target_board = "gimlet-b",
            target_board = "gimlet-c",
            target_board = "sidecar-a",
            target_board = "sidecar-b",
            target_board = "psc-a",
            target_board = "psc-b",
            target_board = "gemini-bu-1"
            ))] {
        const ROT_IRQ: sys_api::PinSet = sys_api::PinSet {
            // On Gemini, the STM32H753 is in a LQFP176 package with ROT_IRQ
            // on pin2/PE3
            port: sys_api::Port::E,
            pin_mask: 1 << 3,
        };
        fn debug_config(_sys: &sys_api::Sys) { }
        fn debug_set(_sys: &sys_api::Sys, _asserted: bool) { }
    } else if #[cfg(target_board = "gimletlet-2")] {
        const ROT_IRQ: sys_api::PinSet = sys_api::PinSet {
            port: sys_api::Port::D,
            pin_mask: 1 << 0,
        };
        const DEBUG_PIN: sys_api::PinSet = sys_api::PinSet {
            port: sys_api::Port::E,
            pin_mask: 1 << 6,
        };
        fn debug_config(sys: &sys_api::Sys) {
            sys.gpio_configure_output(
                DEBUG_PIN,
                sys_api::OutputType::OpenDrain,
                sys_api::Speed::High,
                sys_api::Pull::Up
            ).unwrap_lite();
            debug_set(sys, true);
        }

        fn debug_set(sys: &sys_api::Sys, asserted: bool) {
            ringbuf_entry!(Trace::Debug(asserted));
            sys.gpio_set_to(DEBUG_PIN, asserted).unwrap_lite();
        }
    } else {
        compile_error!("No configuration for ROT_IRQ");
    }
}

/// Return an error if the expected MsgType doesn't match the actual MsgType
fn expect_msg(expected: MsgType, actual: MsgType) -> Result<(), SprotError> {
    if expected != actual {
        ringbuf_entry!(Trace::WrongMsgType(actual));
        Err(SprotError::BadMessageType)
    } else {
        Ok(())
    }
}

pub struct ServerImpl {
    sys: sys_api::Sys,
    spi: drv_spi_api::SpiDevice,

    // Options allow us to `take` the inner messaage and own it in the callee.
    // We then create a new one each time through the dispatch loop.
    pub tx_buf: [u8; BUF_SIZE],
    pub rx_buf: [u8; BUF_SIZE],
}

#[export_name = "main"]
fn main() -> ! {
    let spi = Spi::from(SPI.get_task_id()).device(SP_TO_ROT_SPI_DEVICE);
    let sys = sys_api::Sys::from(SYS.get_task_id());

    sys.gpio_configure_input(ROT_IRQ, sys_api::Pull::None)
        .unwrap_lite();
    debug_config(&sys);

    let mut buffer = [0; idl::INCOMING_SIZE];
    let mut server = ServerImpl {
        sys,
        spi,
        tx_buf: [0u8; BUF_SIZE],
        rx_buf: [0u8; BUF_SIZE],
    };

    loop {
        idol_runtime::dispatch(&mut buffer, &mut server);
    }
}

impl<'a> ServerImpl<'a> {
    // Return a wrapped buffer used for serializing messages
    fn new_txmsg(&mut self) -> TxMsg2 {
        TxMsg2::new(&mut self.tx_buf[..])
    }

    // Return a wrapped buffer used for receiving messages
    fn new_rxmsg(&mut self) -> RxMsg2 {
        RxMsg2::new(&mut self.rx_buf[..])
    }

    // TODO: Move README.md to RFD 317 and discuss:
    //   - Unsolicited messages from RoT to SP.
    //   - Ignoring message from RoT to SP.
    //   - Should we send a message telling RoT that SP has booted?
    //
    // The majority of this is documented in comments in the
    // `ReadState` and `WriteState` enums in `drv/lpc55-sprot-server/
    // src/main.rs` that describes the state machine of the RoT.
    //
    // Any time the SP is sending to the RoT, the RoT should not
    // be asserting ROT_IRQ, as that implies the RoT is sending a
    // reply, which must come after a request. Various scenarios
    // discussed in comments of the lpc55-sprot-server can lead to
    // desynchronoization. The SP forces resynchronization via a CSn
    // pulse, which causes the RoT to deassert ROT_IRQ and go back to
    // waiting for a request.
    //
    // TODO: The RoT must be able to observe SP resets. During the
    // normal start-up seqeunce, the RoT is controlling the SP's boot
    // up sequence. However, the SP can reset itself and individual
    // Hubris tasks may fail and be restarted.
    //
    // If SP and RoT are out of sync, e.g. this task restarts and an old
    // response is still in the RoT's transmit FIFO, then we can also see
    // ROT_IRQ asserted when not expected.
    //
    // TODO: configuration parameters for delays below
    fn handle_unexpected_rot_irq(&self) -> Result<(), SprotError> {
        if self.is_rot_irq_asserted() {
            ringbuf_entry!(Trace::UnexpectedRotIrq);
            // See if the ROT_IRQ completes quickly.
            if !self.wait_rot_irq(false, TIMEOUT_QUICK) {
                ringbuf_entry!(Trace::UnexpectedRotIrq);
                // Nope, it didn't complete. Pulse CSn.
                if self.do_pulse_cs(10_u64, 10_u64)?.rot_irq_end == 1 {
                    ringbuf_entry!(Trace::PulseFailed);
                    // Did not clear ROT_IRQ
                    debug_set(&self.sys, false); // XXX
                    return Err(SprotError::RotNotReady);
                }
            }
        }
        Ok(())
    }

    /// Handle the mechanics of sending a message and waiting for a response.
    fn do_send_recv(
        &mut self,
        txmsg: VerifiedTxMsg2<'a>,
        timeout: u32,
    ) -> Result<VerifiedRxMsg<'a>, SprotError> {
        ringbuf_entry!(Trace::SendRecv(txmsg.data_len()));

        self.handle_unexpected_rot_irq()?;

        let res = self.do_send_request(txmsg);
        _ = self.spi.release(); // TODO: use spi.lock_auto()
        res?;

        if !self.wait_rot_irq(true, timeout) {
            ringbuf_entry!(Trace::RotNotReady);
            return Err(SprotError::RotNotReady);
        }

        ringbuf_entry!(Trace::CSnAssert);
        self.spi.lock(CsState::Asserted)?;
        if PART1_DELAY != 0 {
            hl::sleep_for(PART1_DELAY);
        }

        // Fill in rx_buf with a complete message and validate its crc
        let res = self.do_read_response();

        // We must release the SPI bus before we return
        ringbuf_entry!(Trace::CSnDeassert);
        self.spi.release().map_err(|_| SprotError::SpiServerError)?;

        res
    }

    // Send a request in 2 parts, with optional delays before each part.
    //
    // In order to improve reliability, start by sending only the first
    // ROT_FIFO_SIZE bytes and then delaying a short time. If the RoT is ready,
    // those first bytes will always fit in the RoT receive FIFO. Eventually,
    // the RoT FW will respond to the interrupt and enter a tight loop to
    // receive. The short delay should cover most of the lag in RoT interrupt
    // handling.
    fn do_send_request(
        &mut self,
        msg: VerifiedTxMsg2<'a>,
    ) -> Result<(), SprotError> {
        let part1_len = ROT_FIFO_SIZE.min(msg.len());
        let part1 = &msg.as_slice()[..part1_len];
        let part2 = &msg.as_slice()[part1_len..];
        ringbuf_entry!(Trace::TxPart1(part1.len()));
        ringbuf_entry!(Trace::TxPart2(part2.len()));
        ringbuf_entry!(Trace::CSnAssert);
        self.spi.lock(CsState::Asserted)?;
        if PART1_DELAY != 0 {
            hl::sleep_for(PART1_DELAY);
        }
        self.spi.write(part1)?;
        if !part2.is_empty() {
            hl::sleep_for(PART2_DELAY);
            self.spi.write(part2)?;
        }
        Ok(())
    }

    // Fetch as many bytes as we can and parse the header.
    // Return the parsed header or an error.
    //
    // We can fetch FIFO size number of bytes reliably.
    // After that, a short delay and fetch the rest if there is
    // a payload.
    // Small messages will fit entirely in the RoT FIFO.
    //
    // We don't, but we could speculate that some RoT responses will
    // be longer than ROT_FIFO_SIZE and get ROT_FIFO_SIZE
    // instead of MIN_MSG_SIZE.
    //
    // TODO: Use DMA on RoT to avoid this dance.
    //
    // We know statically that self.rx_buf is large enough to hold
    // part1_len bytes.
    fn do_read_response(&mut self) -> Result<VerifiedRxMsg2<'a>, SprotError> {
        let mut rxmsg = self.rx_msg.take().unwrap_lite();
        let part1_len = MIN_MSG_SIZE.max(ROT_FIFO_SIZE);
        ringbuf_entry!(Trace::RxPart1(part1_len));

        // Read part one
        rxmsg.read(part1_len, |buf| self.spi.read(buf))?;

        let header = self.rx_buf.parse_header(part1_len).map_err(|e| {
            ringbuf_entry!(Trace::ErrWithHeader(e, rxmsg.header_bytes()));
            e
        })?;

        if part1_len < MIN_MSG_SIZE + (header.payload_len as usize) {
            // We haven't read the complete message yet.
            let part2_len =
                MIN_MSG_SIZE + (header.payload_len as usize) - part1_len;
            ringbuf_entry!(Trace::RxPart2(part2_len));

            // Allow RoT time to rouse itself.
            hl::sleep_for(PART2_DELAY);

            // Read part two
            rxmsg.read(part2_len, |buf| self.spi.read(buf))?;
        }

        // This re-does a few inexpensive checks around header validation
        // and parsing, but prevents silly errors and keeps the API small.
        rxmsg.parse().map_err(|(_, e)| e)
    }

    fn do_send_recv_retries(
        &mut self,
        txmsg: VerifiedTxMsg,
        timeout: u32,
        retries: u16,
    ) -> Result<VerifiedRxMsg2<'a>, SprotError> {
        let mut attempts_left = retries;
        let mut errcode = SprotError::Unknown;
        loop {
            if attempts_left == 0 {
                ringbuf_entry!(Trace::FailedRetries { retries, errcode });
                break;
            }
            attempts_left -= 1;

            match self.do_send_recv(txmsg, timeout) {
                // Recoverable errors dealing with our ability to receive
                // the message from the RoT.
                Err(err) => {
                    ringbuf_entry!(Trace::SprotError(err));
                    if is_recoverable_error(err) {
                        errcode = err;
                        hl::sleep_for(RETRY_TIMEOUT);
                        continue;
                    } else {
                        return Err(err);
                    }
                }

                // Intact messages from the RoT may indicate an error on
                // its side.
                Ok(rxmsg) => {
                    match rxmsg.0.msgtype {
                        MsgType::ErrorRsp => {
                            if rxmsg.header().payload_len != 1 {
                                ringbuf_entry!(Trace::ErrRspPayloadSize(
                                    rxmsg.header().payload_len
                                ));
                                // Treat this as a recoverable error
                                hl::sleep_for(RETRY_TIMEOUT);
                                ringbuf_entry!(Trace::ErrRespNoPayload);
                                continue;
                            }
                            errcode = SprotError::from(rxmsg.payload()[0]);
                            ringbuf_entry!(Trace::SprotError(errcode));
                            if is_recoverable_error(errcode) {
                                // TODO: There are rare cases where
                                // the RoT dose not receive
                                // a 0x01 as the first byte in a message.
                                // See issue #929.
                                hl::sleep_for(RETRY_TIMEOUT);
                                ringbuf_entry!(Trace::Recoverable(errcode));
                                continue;
                            }
                            // Other errors from RoT are not recoverable with
                            // a retry.
                            return Err(errcode);
                        }
                        // All of the non-error message types are ok here.
                        _ => return Ok(rxmsg),
                    }
                }
            }
        }
        Err(errcode)
    }

    /// Clear the ROT_IRQ and the RoT's Tx buffer by toggling the CSn signal.
    /// ROT_IRQ before and after state is returned for testing.
    fn do_pulse_cs(
        &mut self,
        delay: u64,
        delay_after: u64,
    ) -> Result<PulseStatus, SprotError> {
        let rot_irq_begin = self.is_rot_irq_asserted();
        ringbuf_entry!(Trace::CSnAssert);
        self.spi
            .lock(CsState::Asserted)
            .map_err(|_| SprotError::CannotAssertCSn)?;
        if delay != 0 {
            hl::sleep_for(delay);
        }
        ringbuf_entry!(Trace::CSnDeassert);
        self.spi.release().unwrap_lite();
        if delay_after != 0 {
            hl::sleep_for(delay_after);
        }
        let rot_irq_end = self.is_rot_irq_asserted();
        let status = PulseStatus {
            rot_irq_begin: u8::from(rot_irq_begin),
            rot_irq_end: u8::from(rot_irq_end),
        };
        Ok(status)
    }

    fn is_rot_irq_asserted(&mut self) -> bool {
        self.sys.gpio_read(ROT_IRQ).unwrap_lite() == 0
    }

    // Poll ROT_IRQ until asserted (true) or deasserted (false).
    //
    // We sleep and poll for what should be long enough for the RoT to queue
    // a response.
    //
    // TODO: Use STM32 EXTI as  an interrupt allows for better performance and
    // power efficiency.
    //
    // STM32 EXTI allows for 16 interrupts for GPIOs.
    // Each of those can represent Pin X from a GPIO bank (A through K)
    // So, only one bank's Pin 3, for example, can have the #3 interrupt.
    // For ROT_IRQ, we would configure for the falling edge to trigger
    // the interrupt. That configuration should be specified in the app.toml
    // for the board. Work needs to be done to generalize the EXTI facility.
    // But, hacking in one interrupt as an example should be ok to start things
    // off.
    fn wait_rot_irq(&mut self, desired: bool, max_sleep: u32) -> bool {
        let mut slept = 0;
        while self.is_rot_irq_asserted() != desired {
            if slept == max_sleep {
                ringbuf_entry!(Trace::RotReadyTimeout);
                return false;
            }
            hl::sleep_for(1);
            slept += 1;
        }
        true
    }

    fn upd(
        &mut self,
        txmsg: VerifiedTxMsg2,
        rsp: MsgType,
        timeout: u32,
        attempts: u16,
    ) -> Result<Option<u32>, SprotError> {
        let rxmsg = self.do_send_recv_retries(txmsg, timeout, attempts)?;

        if rxmsg.0.msgtype == rsp {
            let rsp = self
                .rx_buf
                .deserialize_hubpack_payload::<UpdateRspHeader>(&rxmsg)?;
            ringbuf_entry!(Trace::UpdResponse(rsp));
            rsp.map_err(|e: u32| {
                UpdateError::try_from(e)
                    .unwrap_or(UpdateError::Unknown)
                    .into()
            })
        } else {
            expect_msg(MsgType::ErrorRsp, rxmsg.0.msgtype)?;
            if rxmsg.0.payload_len != 1 {
                return Err(SprotError::BadMessageLength);
            }
            let payload = self.rx_buf.payload(&rxmsg);
            let err = SprotError::from(payload[0]);
            ringbuf_entry!(Trace::Error(err));
            Err(err)
        }
    }
}

impl idl::InOrderSpRotImpl for ServerImpl {
    /// Send a message to the RoT for processing.
    fn send_recv(
        &mut self,
        recv_msg: &RecvMessage,
        msgtype: drv_sprot_api::MsgType,
        source: Leased<R, [u8]>,
        sink: Leased<W, [u8]>,
    ) -> Result<Received, RequestError<SprotError>> {
        self.send_recv_retries(recv_msg, msgtype, 1, source, sink)
    }

    /// Send a message to the RoT for processing.
    fn send_recv_retries(
        &mut self,
        _: &RecvMessage,
        msgtype: drv_sprot_api::MsgType,
        attempts: u16,
        source: Leased<R, [u8]>,
        sink: Leased<W, [u8]>,
    ) -> Result<Received, RequestError<SprotError>> {
        let txmsg = self.new_txmsg();
        let verified_txmsg = txmsg.from_lease(msgtype, source)?;

        match self.do_send_recv_retries(verified_txmsg, TIMEOUT_MAX, attempts) {
            Ok(rxmsg) => {
                let payload = rxmsg.payload();
                if !payload.is_empty() {
                    sink.write_range(0..payload.len(), payload).map_err(
                        |_| RequestError::Fail(ClientError::WentAway),
                    )?;
                }
                Ok(Received {
                    length: u16::try_from(payload.len()).unwrap_lite(),
                    msgtype: msgtype as u8,
                })
            }
            Err(err) => Err(idol_runtime::RequestError::Runtime(err)),
        }
    }

    /// Clear the RoT Tx buffer and have the RoT deassert ROT_IRQ.
    /// The status of ROT_IRQ before and after the assert is returned.
    ///
    /// If ROT_IRQ is asserted (a response is pending)
    /// ROT_IRQ should be deasserted in response to CSn pulse.
    fn pulse_cs(
        &mut self,
        _: &RecvMessage,
        delay: u16,
    ) -> Result<PulseStatus, RequestError<SprotError>> {
        self.do_pulse_cs(delay.into(), delay.into())
            .map_err(|e| e.into())
    }

    cfg_if::cfg_if! {
        if #[cfg(feature = "sink_test")] {
            /// Send `count` buffers of `size` size to simulate a firmare
            /// update or other bulk data transfer from the SP to the RoT.
            //
            // The RoT will read all of the bytes of a MsgType::SinkReq and
            // include the received sequence number in its SinkRsp message.
            //
            // The RoT reports errors in an ErrorRsp message.
            //
            // For the sake of working with a logic analyzer,
            // a known pattern is put into the SinkReq messages so that
            // most of the received bytes match their buffer index modulo
            // 0x100.
            //
            fn rot_sink(
                &mut self,
                _: &RecvMessage,
                count: u16,
                size: u16,
            ) -> Result<SinkStatus, RequestError<SprotError>> {
                let size = size as usize;
                debug_set(&self.sys, false);

                let mut sent = 0u16;
                let result = loop {
                    if sent == count {
                        break Ok(sent);
                    }
                    let mut txmsg = self.new_txmsg();
                    ringbuf_entry!(Trace::SinkLoop(sent));

                    match txmsg.sink_req(size, sent) {
                        Err(err) => break Err(err),
                        Ok(verified_txmsg) => {
                            match self.do_send_recv_retries(verified_txmsg, TIMEOUT_QUICK, MAX_SINKREQ_ATTEMPTS) {
                                Err(err) => {
                                    ringbuf_entry!(Trace::SinkFail(err, sent));
                                    break Err(err)
                                },
                                Ok(rxmsg) => {
                                    match rxmsg.header().msgtype {
                                        MsgType::SinkRsp => {
                                            // TODO: Check sequence number in response.
                                            if rxmsg.payload().len() as usize >= core::mem::size_of::<u16>() {
                                                let seq_buf = &rxmsg.payload()[..core::mem::size_of::<u16>()];
                                                let r_seqno = LittleEndian::read_u16(seq_buf);
                                                if sent != r_seqno {
                                                    break Err(SprotError::Sequence);
                                                }
                                            }
                                        },
                                        MsgType::ErrorRsp => {
                                            if rxmsg.payload.len() != 1 {
                                                break Err(SprotError::BadMessageLength);
                                            }
                                            break Err(SprotError::from(rxmsg.payload[0]));
                                        },
                                        _ => {
                                            // Other non-SinkRsp messages from the RoT
                                            // are not recoverable with a retry.
                                            break Err(SprotError::BadMessageType);
                                        },
                                    }
                                },
                            }
                            sent = sent.wrapping_add(1);
                        },
                    }
                };
                debug_set(&self.sys, true);
                match result {
                    Ok(sent) => {
                        Ok(SinkStatus { sent })
                    },
                    Err(err) => {
                        Err(RequestError::Runtime(err))
                    },
                }
            }
        } else {
            fn rot_sink(
                &mut self,
                _: &RecvMessage,
                _count: u16,
                _size: u16,
            ) -> Result<SinkStatus, RequestError<SprotError>> {
                Err(RequestError::Runtime(SprotError::NotImplemented))
            }
        }
    }

    /// Retrieve status from the RoT.
    ///
    /// Use trusted interfaces when available. This is meant as
    /// an early or fallback source of information prior to stronger
    /// levels of trust being established.
    /// Having a signed StatusRsp is possible, but consider that carefully.
    fn status(
        &mut self,
        _: &RecvMessage,
    ) -> Result<SprotStatus, RequestError<SprotError>> {
        let txmsg = self.new_txmsg().no_payload(MsgType::StatusReq);
        let rxmsg = self.do_send_recv(txmsg, TIMEOUT_QUICK)?;
        expect_msg(MsgType::StatusRsp, rxmsg.header().msgtype)?;
        let status = rxmsg.deserialize_hubpack_payload::<SprotStatus>()?;
        Ok(status)
    }

    fn io_stats(
        &mut self,
        _: &RecvMessage,
    ) -> Result<IoStats, RequestError<SprotError>> {
        let txmsg = self.new_txmsg().no_payload(MsgType::IoStatsReq);
        let rxmsg = self.do_send_recv(txmsg, TIMEOUT_QUICK)?;
        expect_msg(MsgType::IoStatsRsp, rxmsg.0.msgtype)?;
        let status = rxmsg.deserialize_hubpack_payload::<IoStats>()?;
        Ok(status)
    }

    fn block_size(
        &mut self,
        _msg: &userlib::RecvMessage,
    ) -> Result<usize, RequestError<SprotError>> {
        let txmsg = self.new_txmsg().no_payload(MsgType::UpdBlockSizeReq);
        match self.upd(txmsg, MsgType::UpdBlockSizeRsp, TIMEOUT_QUICK, 1)? {
            Some(block_size) => {
                let bs = block_size as usize;
                ringbuf_entry!(Trace::BlockSize(bs));
                Ok(bs)
            }
            None => Err(idol_runtime::RequestError::Runtime(
                SprotError::UpdateSpRotError,
            )),
        }
    }

    fn prep_image_update(
        &mut self,
        _msg: &userlib::RecvMessage,
        image_type: UpdateTarget,
    ) -> Result<(), idol_runtime::RequestError<SprotError>> {
        ringbuf_entry!(Trace::UpdatePrep);
        let txmsg = self
            .new_txmsg()
            .serialize(MsgType::UpdPrepImageUpdateReq, image_type)
            .map_err(|(_, e)| SprotError::from(e))?;
        let _ =
            self.upd(txmsg, MsgType::UpdPrepImageUpdateRsp, TIMEOUT_QUICK, 1)?;
        Ok(())
    }

    fn write_one_block(
        &mut self,
        _msg: &userlib::RecvMessage,
        block_num: u32,
        // XXX Is a separate length needed here? Lease always 1024 even if not all used?
        // XXX 1024 needs to come from somewhere.
        block: idol_runtime::LenLimit<
            idol_runtime::Leased<idol_runtime::R, [u8]>,
            1024,
        >,
    ) -> Result<(), idol_runtime::RequestError<SprotError>> {
        ringbuf_entry!(Trace::UpdateWriteOneBlock);
        let txmsg = self.new_txmsg().block(block_num, block)?;

        let _ = self.upd(
            txmsg,
            MsgType::UpdWriteOneBlockRsp,
            TIMEOUT_WRITE_ONE_BLOCK,
            MAX_UPDATE_ATTEMPTS,
        )?;
        Ok(())
    }

    fn finish_image_update(
        &mut self,
        _msg: &userlib::RecvMessage,
    ) -> Result<(), idol_runtime::RequestError<SprotError>> {
        let txmsg = self
            .new_txmsg()
            .no_payload(MsgType::UpdFinishImageUpdateReq);
        let _ = self.upd(
            txmsg,
            MsgType::UpdFinishImageUpdateRsp,
            TIMEOUT_QUICK,
            1,
        )?;
        Ok(())
    }

    fn abort_update(
        &mut self,
        _msg: &userlib::RecvMessage,
    ) -> Result<(), idol_runtime::RequestError<SprotError>> {
        let txmsg = self.new_txmsg().no_payload(MsgType::UpdAbortUpdateReq);
        let _ =
            self.upd(txmsg, MsgType::UpdAbortUpdateRsp, TIMEOUT_QUICK, 1)?;
        Ok(())
    }
}

mod idl {
    use super::{
        IoStats, MsgType, PulseStatus, Received, SinkStatus, SprotError,
        SprotStatus, UpdateTarget,
    };

    include!(concat!(env!("OUT_DIR"), "/server_stub.rs"));
}
