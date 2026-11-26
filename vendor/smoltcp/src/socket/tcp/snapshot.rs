//! Exporting and importing a TCP socket's whole state, for a program that
//! saves its connections to restore them later, or elsewhere (a virtual
//! machine's network suspended to a file and resumed).
//!
//! [`Socket::snapshot`] gives a [`SocketSnapshot`]: plain data, with every
//! field public, that the program encodes however it likes.
//! [`Socket::restore`] puts one back into a socket, which then carries on
//! the connection where the original left it: the same state, endpoints,
//! sequence numbers, windows, options and data in both directions. Points
//! in time are kept relative to the `now` each is given, so that a socket
//! restored by another clock resumes its timers with the time they had
//! left.
//!
//! Two things are configuration rather than state, and are not carried:
//! the timestamp generator (a function: the restoring socket's own is used
//! if the connection negotiated timestamps), and the congestion
//! controller's internal state (the algorithm is carried, and restarts as
//! for a new connection).

use super::{AckDelayTimer, CongestionControl, RttEstimator, Socket, State, Timer, Tuple};
use crate::storage::Assembler;
use crate::time::{Duration, Instant};
use crate::wire::{IpEndpoint, IpListenEndpoint, TcpSeqNumber};

use crate::storage::RingBuffer;
use alloc::vec::Vec;

/// A point in time, as microseconds after the `now` the snapshot was taken
/// at (negative: before it).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TimeOffset(pub i64);

impl TimeOffset {
    fn from_instant(at: Instant, now: Instant) -> Self {
        Self(at.total_micros() - now.total_micros())
    }

    fn to_instant(self, now: Instant) -> Instant {
        Instant::from_micros(now.total_micros() + self.0)
    }
}

/// The socket's retransmission, keep-alive or close timer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TimerSnapshot {
    Idle {
        keep_alive_at: Option<TimeOffset>,
    },
    Retransmit {
        expires_at: TimeOffset,
        delay: Duration,
    },
    FastRetransmit,
    Close {
        expires_at: TimeOffset,
    },
}

/// The delayed acknowledgement timer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AckDelaySnapshot {
    Idle,
    Waiting(TimeOffset),
    Immediate,
}

/// The round-trip time estimate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RttSnapshot {
    /// Milliseconds.
    pub rtt: u32,
    /// Milliseconds.
    pub deviation: u32,
    /// The segment being timed, and when it was sent.
    pub timing: Option<(TimeOffset, TcpSeqNumber)>,
    pub max_seq_sent: Option<TcpSeqNumber>,
    pub rto_count: u8,
}

/// Out-of-order data received: `data` begins `offset` bytes past the end
/// of [`SocketSnapshot::rx_data`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Segment {
    pub offset: usize,
    pub data: Vec<u8>,
}

/// A TCP socket's state, as plain data. The field names follow the
/// socket's own; see its documentation for what each means.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SocketSnapshot {
    pub state: State,
    pub timer: TimerSnapshot,
    pub rtt: RttSnapshot,
    /// The receive buffer's capacity: the window advertised was computed
    /// from it, so a restoring socket's buffer must be at least as large.
    pub rx_capacity: usize,
    /// Data received in order and not yet read.
    pub rx_data: Vec<u8>,
    /// Data received beyond a gap, waiting for it to be filled.
    pub rx_segments: Vec<Segment>,
    pub rx_fin_received: bool,
    /// Data queued for sending: sent and unacknowledged, then unsent.
    pub tx_data: Vec<u8>,
    pub timeout: Option<Duration>,
    pub keep_alive: Option<Duration>,
    pub hop_limit: Option<u8>,
    pub listen_endpoint: IpListenEndpoint,
    /// The local and remote endpoints, if connected.
    pub tuple: Option<(IpEndpoint, IpEndpoint)>,
    pub local_seq_no: TcpSeqNumber,
    pub remote_seq_no: TcpSeqNumber,
    pub remote_last_seq: TcpSeqNumber,
    pub remote_last_ack: Option<TcpSeqNumber>,
    pub remote_last_win: u16,
    pub remote_win_shift: u8,
    pub remote_win_len: usize,
    pub remote_win_scale: Option<u8>,
    pub remote_has_sack: bool,
    pub remote_mss: usize,
    pub remote_last_ts: Option<TimeOffset>,
    pub local_rx_last_seq: Option<TcpSeqNumber>,
    pub local_rx_last_ack: Option<TcpSeqNumber>,
    pub local_rx_dup_acks: u8,
    pub ack_delay: Option<Duration>,
    pub ack_delay_timer: AckDelaySnapshot,
    pub challenge_ack_timer: TimeOffset,
    pub nagle: bool,
    pub congestion_control: CongestionControl,
    /// Whether the connection uses TCP timestamps (RFC 7323).
    pub timestamps: bool,
    pub last_remote_tsval: u32,
}

/// Why a snapshot cannot be restored into a socket.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RestoreError {
    /// A buffer of the socket is smaller than the snapshot needs.
    BufferTooSmall,
    /// The out-of-order data does not fit the receive window, or has more
    /// gaps than the reassembler tracks.
    BadSegments,
    /// The connection uses timestamps, and the socket has no generator.
    NoTimestampGenerator,
}

impl core::fmt::Display for RestoreError {
    fn fmt(&self, f: &mut core::fmt::Formatter) -> core::fmt::Result {
        match self {
            Self::BufferTooSmall => write!(f, "a socket buffer is too small for the snapshot"),
            Self::BadSegments => write!(f, "the snapshot's out-of-order data is invalid"),
            Self::NoTimestampGenerator => {
                write!(
                    f,
                    "the connection uses timestamps and the socket has no generator"
                )
            }
        }
    }
}

#[cfg(feature = "std")]
impl std::error::Error for RestoreError {}

impl<'a> Socket<'a> {
    /// The socket's whole state, with its points in time relative to `now`.
    pub fn snapshot(&self, now: Instant) -> SocketSnapshot {
        let rel = |at: Instant| TimeOffset::from_instant(at, now);
        let rx_segments = self
            .assembler
            .iter_data(0)
            .map(|(start, end)| {
                let mut data = alloc::vec![0; end - start];
                let read = self.rx_buffer.peek_unallocated(start, &mut data);
                data.truncate(read);
                Segment {
                    offset: start,
                    data,
                }
            })
            .collect();

        SocketSnapshot {
            state: self.state,
            timer: match self.timer {
                Timer::Idle { keep_alive_at } => TimerSnapshot::Idle {
                    keep_alive_at: keep_alive_at.map(rel),
                },
                Timer::Retransmit { expires_at, delay } => TimerSnapshot::Retransmit {
                    expires_at: rel(expires_at),
                    delay,
                },
                Timer::FastRetransmit => TimerSnapshot::FastRetransmit,
                Timer::Close { expires_at } => TimerSnapshot::Close {
                    expires_at: rel(expires_at),
                },
            },
            rtt: RttSnapshot {
                rtt: self.rtte.rtt,
                deviation: self.rtte.deviation,
                timing: self.rtte.timestamp.map(|(at, seq)| (rel(at), seq)),
                max_seq_sent: self.rtte.max_seq_sent,
                rto_count: self.rtte.rto_count,
            },
            rx_capacity: self.rx_buffer.capacity(),
            rx_data: allocated(&self.rx_buffer),
            rx_segments,
            rx_fin_received: self.rx_fin_received,
            tx_data: allocated(&self.tx_buffer),
            timeout: self.timeout,
            keep_alive: self.keep_alive,
            hop_limit: self.hop_limit,
            listen_endpoint: self.listen_endpoint,
            tuple: self.tuple.map(|t| (t.local, t.remote)),
            local_seq_no: self.local_seq_no,
            remote_seq_no: self.remote_seq_no,
            remote_last_seq: self.remote_last_seq,
            remote_last_ack: self.remote_last_ack,
            remote_last_win: self.remote_last_win,
            remote_win_shift: self.remote_win_shift,
            remote_win_len: self.remote_win_len,
            remote_win_scale: self.remote_win_scale,
            remote_has_sack: self.remote_has_sack,
            remote_mss: self.remote_mss,
            remote_last_ts: self.remote_last_ts.map(rel),
            local_rx_last_seq: self.local_rx_last_seq,
            local_rx_last_ack: self.local_rx_last_ack,
            local_rx_dup_acks: self.local_rx_dup_acks,
            ack_delay: self.ack_delay,
            ack_delay_timer: match self.ack_delay_timer {
                AckDelayTimer::Idle => AckDelaySnapshot::Idle,
                AckDelayTimer::Waiting(at) => AckDelaySnapshot::Waiting(rel(at)),
                AckDelayTimer::Immediate => AckDelaySnapshot::Immediate,
            },
            challenge_ack_timer: rel(self.challenge_ack_timer),
            nagle: self.nagle,
            congestion_control: self.congestion_control(),
            timestamps: self.tsval_generator.is_some(),
            last_remote_tsval: self.last_remote_tsval,
        }
    }

    /// Replaces the socket's state with `snapshot`'s, its points in time
    /// now relative to `now`. The socket keeps its buffers (which must be
    /// large enough) and its timestamp generator (which a connection using
    /// timestamps needs). On an error the socket is unchanged.
    pub fn restore(&mut self, snapshot: &SocketSnapshot, now: Instant) -> Result<(), RestoreError> {
        let s = snapshot;
        if self.rx_buffer.capacity() < s.rx_capacity
            || self.rx_buffer.capacity() < s.rx_data.len()
            || self.tx_buffer.capacity() < s.tx_data.len()
        {
            return Err(RestoreError::BufferTooSmall);
        }
        let window = self.rx_buffer.capacity() - s.rx_data.len();
        let mut assembler = Assembler::new();
        for segment in &s.rx_segments {
            let end = segment.offset.checked_add(segment.data.len());
            if segment.data.is_empty() || end.map_or(true, |end| end > window) {
                return Err(RestoreError::BadSegments);
            }
            if assembler.add(segment.offset, segment.data.len()).is_err() {
                return Err(RestoreError::BadSegments);
            }
        }
        if s.timestamps && self.tsval_generator.is_none() {
            return Err(RestoreError::NoTimestampGenerator);
        }

        let abs = |at: TimeOffset| at.to_instant(now);
        self.rx_buffer.clear();
        let _ = self.rx_buffer.enqueue_slice(&s.rx_data);
        for segment in &s.rx_segments {
            let _ = self
                .rx_buffer
                .write_unallocated(segment.offset, &segment.data);
        }
        self.assembler = assembler;
        self.tx_buffer.clear();
        let _ = self.tx_buffer.enqueue_slice(&s.tx_data);

        self.state = s.state;
        self.timer = match s.timer {
            TimerSnapshot::Idle { keep_alive_at } => Timer::Idle {
                keep_alive_at: keep_alive_at.map(abs),
            },
            TimerSnapshot::Retransmit { expires_at, delay } => Timer::Retransmit {
                expires_at: abs(expires_at),
                delay,
            },
            TimerSnapshot::FastRetransmit => Timer::FastRetransmit,
            TimerSnapshot::Close { expires_at } => Timer::Close {
                expires_at: abs(expires_at),
            },
        };
        self.rtte = RttEstimator {
            rtt: s.rtt.rtt,
            deviation: s.rtt.deviation,
            timestamp: s.rtt.timing.map(|(at, seq)| (abs(at), seq)),
            max_seq_sent: s.rtt.max_seq_sent,
            rto_count: s.rtt.rto_count,
        };
        self.rx_fin_received = s.rx_fin_received;
        self.timeout = s.timeout;
        self.keep_alive = s.keep_alive;
        self.hop_limit = s.hop_limit;
        self.listen_endpoint = s.listen_endpoint;
        self.tuple = s.tuple.map(|(local, remote)| Tuple { local, remote });
        self.local_seq_no = s.local_seq_no;
        self.remote_seq_no = s.remote_seq_no;
        self.remote_last_seq = s.remote_last_seq;
        self.remote_last_ack = s.remote_last_ack;
        self.remote_last_win = s.remote_last_win;
        self.remote_win_shift = s.remote_win_shift;
        self.remote_win_len = s.remote_win_len;
        self.remote_win_scale = s.remote_win_scale;
        self.remote_has_sack = s.remote_has_sack;
        self.remote_mss = s.remote_mss;
        self.remote_last_ts = s.remote_last_ts.map(abs);
        self.local_rx_last_seq = s.local_rx_last_seq;
        self.local_rx_last_ack = s.local_rx_last_ack;
        self.local_rx_dup_acks = s.local_rx_dup_acks;
        self.ack_delay = s.ack_delay;
        self.ack_delay_timer = match s.ack_delay_timer {
            AckDelaySnapshot::Idle => AckDelayTimer::Idle,
            AckDelaySnapshot::Waiting(at) => AckDelayTimer::Waiting(abs(at)),
            AckDelaySnapshot::Immediate => AckDelayTimer::Immediate,
        };
        self.challenge_ack_timer = abs(s.challenge_ack_timer);
        self.nagle = s.nagle;
        self.set_congestion_control(s.congestion_control);
        let controller = self.congestion_controller.inner_mut();
        controller.set_mss(s.remote_mss);
        controller.set_remote_window(s.remote_win_len);
        if !s.timestamps {
            self.tsval_generator = None;
        }
        self.last_remote_tsval = s.last_remote_tsval;

        #[cfg(feature = "async")]
        {
            self.rx_waker.wake();
            self.tx_waker.wake();
        }
        Ok(())
    }
}

/// The data in a ring buffer, in order.
fn allocated(buffer: &RingBuffer<'_, u8>) -> Vec<u8> {
    let first = buffer.get_allocated(0, buffer.len());
    let mut data = first.to_vec();
    data.extend_from_slice(buffer.get_allocated(first.len(), buffer.len()));
    data
}
