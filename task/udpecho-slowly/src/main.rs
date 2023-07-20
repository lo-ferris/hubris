// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

#![no_std]
#![no_main]

use drv_busywait_api::Busywait;
use task_net_api::*;
use userlib::*;

task_slot!(NET, net);
task_slot!(BUSYWAIT, busywait);

#[export_name = "main"]
fn main() -> ! {
    let net = NET.get_task_id();
    let net = Net::from(net);
    let busywait = BUSYWAIT.get_task_id();
    let busywait = Busywait::from(busywait);

    const SOCKET: SocketName = SocketName::echo_slowly;

    loop {
        // Tiiiiiny payload buffer
        let mut rx_data_buf = [0u8; 16];
        match net.recv_packet(
            SOCKET,
            LargePayloadBehavior::Discard,
            &mut rx_data_buf,
        ) {
            Ok(meta) => {
                // A packet! We want to turn it right around. Deserialize the
                // packet header; unwrap because we trust the server.
                UDP_ECHO_SLOWLY_COUNT
                    .fetch_add(1, core::sync::atomic::Ordering::Relaxed);

                // Now we know how many bytes to return.
                let tx_bytes = &rx_data_buf[..meta.size as usize];

                if tx_bytes.starts_with(b"1") {
                    busywait.spin();
                }

                loop {
                    match net.send_packet(SOCKET, meta, tx_bytes) {
                        Ok(()) => break,
                        Err(SendError::QueueFull) => {
                            // Our outgoing queue is full; wait for space.
                            sys_recv_closed(&mut [], 1, TaskId::KERNEL)
                                .unwrap();
                        }
                        Err(SendError::NotYours) => panic!(),
                        Err(SendError::InvalidVLan) => panic!(),
                        Err(SendError::Other) => panic!(),
                        Err(SendError::ServerRestarted) => (),
                    }
                }
            }
            Err(RecvError::QueueEmpty) => {
                // Our incoming queue is empty. Wait for more packets.
                sys_recv_closed(&mut [], 1, TaskId::KERNEL).unwrap();
            }
            Err(RecvError::NotYours) => panic!(),
            Err(RecvError::Other) => panic!(),
            Err(RecvError::ServerRestarted) => (),
        }

        // Try again.
    }
}

static UDP_ECHO_SLOWLY_COUNT: core::sync::atomic::AtomicU32 =
    core::sync::atomic::AtomicU32::new(0);