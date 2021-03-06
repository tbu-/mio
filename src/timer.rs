use {convert, io, Evented, EventSet, Poll, PollOpt, Registration, SetReadiness, Token};
use lazy::Lazy;
use std::{cmp, error, fmt, u64, usize, iter, thread};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use self::TimerErrorKind::TimerOverflow;

pub struct Timer<T> {
    // Size of each tick in milliseconds
    tick_ms: u64,
    // Slab of timeout entries
    entries: Slab<Entry<T>>,
    // Timeout wheel. Each tick, the timer will look at the next slot for
    // timeouts that match the current tick.
    wheel: Vec<WheelEntry>,
    // Tick 0's time in milliseconds
    start: u64,
    // The current tick
    tick: Tick,
    // The next entry to possibly timeout
    next: Token,
    // Masks the target tick to get the slot
    mask: u64,
    // Set on registration with Poll
    inner: Lazy<Inner>,
}

pub struct Builder {
    // Approximate duration of each tick
    tick: Duration,
    // Number of slots in the timer wheel
    num_slots: usize,
    // Max number of timeouts that can be in flight at a given time.
    capacity: usize,
}

#[derive(Clone, Debug)]
pub struct Timeout {
    // Reference into the timer entry slab
    token: Token,
    // Tick that it should matchup with
    tick: u64,
}

struct Inner {
    registration: Registration,
    set_readiness: SetReadiness,
    wakeup_state: WakeupState,
    wakeup_thread: thread::JoinHandle<()>,
}

#[derive(Copy, Clone, Debug)]
struct WheelEntry {
    next_tick: Tick,
    head: Token,
}

// Doubly linked list of timer entries. Allows for efficient insertion /
// removal of timeouts.
struct Entry<T> {
    state: T,
    links: EntryLinks,
}

#[derive(Copy, Clone)]
struct EntryLinks {
    tick: Tick,
    prev: Token,
    next: Token
}

type Tick = u64;

const TICK_MAX: Tick = u64::MAX;

// Manages communication with wakeup thread
type WakeupState = Arc<AtomicUsize>;

type Slab<T> = ::slab::Slab<T, ::Token>;

pub type Result<T> = ::std::result::Result<T, TimerError>;
// TODO: remove
pub type TimerResult<T> = Result<T>;


#[derive(Debug)]
pub struct TimerError {
    kind: TimerErrorKind,
    desc: &'static str,
}

#[derive(Debug)]
pub enum TimerErrorKind {
    TimerOverflow,
}

// TODO: Remove
pub type OldTimerResult<T> = Result<T>;

const TERMINATE_THREAD: usize = 0;
const EMPTY: Token = Token(usize::MAX);
const NS_PER_MS: u32 = 1_000_000;

impl Builder {
    pub fn tick_duration(mut self, duration: Duration) -> Builder {
        self.tick = duration;
        self
    }

    pub fn num_slots(mut self, num_slots: usize) -> Builder {
        self.num_slots = num_slots;
        self
    }

    pub fn capacity(mut self, capacity: usize) -> Builder {
        self.capacity = capacity;
        self
    }

    pub fn build<T>(self) -> Timer<T> {
        Timer::new(convert::millis(self.tick), self.num_slots, self.capacity, now_ms())
    }
}

impl Default for Builder {
    fn default() -> Builder {
        Builder {
            tick: Duration::from_millis(100),
            num_slots: 256,
            capacity: 65_536,
        }
    }
}

impl<T> Timer<T> {
    fn new(tick_ms: u64, num_slots: usize, capacity: usize, start: u64) -> Timer<T> {
        let num_slots = num_slots.next_power_of_two();
        let capacity = capacity.next_power_of_two();
        let mask = (num_slots as u64) - 1;
        let wheel = iter::repeat(WheelEntry { next_tick: TICK_MAX, head: EMPTY })
            .take(num_slots).collect();

        Timer {
            tick_ms: tick_ms,
            entries: Slab::new(capacity),
            wheel: wheel,
            start: start,
            tick: 0,
            next: EMPTY,
            mask: mask,
            inner: Lazy::new(),
        }
    }

    pub fn set_timeout(&mut self, delay: Duration, state: T) -> Result<Timeout> {
        let at = now_ms() + convert::millis(delay);
        self.set_timeout_at(at, state)
    }

    fn set_timeout_at(&mut self, mut delay: u64, state: T) -> Result<Timeout> {
        // Make relative to start
        delay -= self.start;

        // Calculate tick
        let mut tick = delay / self.tick_ms;

        // Ensure rounded to the closest tick
        if delay % self.tick_ms >= self.tick_ms / 2 {
            tick += 1;
        }

        trace!("setting timeout; delay={:?}; tick={:?}; current-tick={:?}", delay, tick, self.tick);

        // Always target at least 1 tick in the future
        if tick <= self.tick {
            tick = self.tick + 1;
        }

        self.insert(tick, state)
    }

    fn insert(&mut self, tick: Tick, state: T) -> Result<Timeout> {
        // Get the slot for the requested tick
        let slot = (tick & self.mask) as usize;
        let curr = self.wheel[slot];

        // Insert the new entry
        let token = try!(
            self.entries.insert(Entry::new(state, tick, curr.head))
            .map_err(|_| TimerError::overflow()));

        if curr.head != EMPTY {
            // If there was a previous entry, set its prev pointer to the new
            // entry
            self.entries[curr.head].links.prev = token;
        }

        // Update the head slot
        self.wheel[slot] = WheelEntry {
            next_tick: cmp::min(tick, curr.next_tick),
            head: token,
        };

        self.schedule_readiness(tick);

        trace!("inserted timout; slot={}; token={:?}", slot, token);

        // Return the new timeout
        Ok(Timeout {
            token: token,
            tick: tick
        })
    }

    pub fn cancel_timeout(&mut self, timeout: &Timeout) -> Option<T> {
        let links = match self.entries.get(timeout.token) {
            Some(e) => e.links,
            None => return None
        };

        // Sanity check
        if links.tick != timeout.tick {
            return None;
        }

        self.unlink(&links, timeout.token);
        self.entries.remove(timeout.token).map(|e| e.state)
    }

    pub fn poll(&mut self) -> Option<T> {
        let target_tick = self.current_tick();
        self.poll_to(target_tick)
    }

    fn poll_to(&mut self, target_tick: Tick) -> Option<T> {
        trace!("tick_to; target_tick={}; current_tick={}", target_tick, self.tick);

        while self.tick <= target_tick {
            let curr = self.next;

            trace!("ticking; curr={:?}", curr);

            if curr == EMPTY {
                self.tick += 1;
                self.next = self.wheel[self.slot_for(self.tick)].head;
            } else {
                let slot = self.slot_for(self.tick);

                if curr == self.wheel[slot].head {
                    self.wheel[slot].next_tick = TICK_MAX;
                }

                let links = self.entries[curr].links;

                if links.tick <= self.tick {
                    trace!("triggering; token={:?}", curr);

                    // Unlink will also advance self.next
                    self.unlink(&links, curr);

                    // Remove and return the token
                    return self.entries.remove(curr)
                        .map(|e| e.state);
                } else {
                    let next_tick = self.wheel[slot].next_tick;
                    self.wheel[slot].next_tick = cmp::min(next_tick, links.tick);
                    self.next = links.next;
                }
            }
        }

        // No more timeouts to poll
        if let Some(inner) = self.inner.as_ref() {
            trace!("unsetting readiness");
            let _ = inner.set_readiness.set_readiness(EventSet::none());

            if let Some(tick) = self.next_tick() {
                self.schedule_readiness(tick);
            }
        }

        None
    }

    fn unlink(&mut self, links: &EntryLinks, token: Token) {
       trace!("unlinking timeout; slot={}; token={:?}",
               self.slot_for(links.tick), token);

        if links.prev == EMPTY {
            let slot = self.slot_for(links.tick);
            self.wheel[slot].head = links.next;
        } else {
            self.entries[links.prev].links.next = links.next;
        }

        if links.next != EMPTY {
            self.entries[links.next].links.prev = links.prev;

            if token == self.next {
                self.next = links.next;
            }
        } else if token == self.next {
            self.next = EMPTY;
        }
    }

    fn schedule_readiness(&self, tick: Tick) {
        if let Some(inner) = self.inner.as_ref() {
            // Coordinate setting readiness w/ the wakeup thread
            let mut curr = inner.wakeup_state.load(Ordering::Acquire);

            loop {
                if curr as Tick <= tick {
                    // Nothing to do, wakeup is already scheduled
                    return;
                }

                // Attempt to move the wakeup time forward
                let actual = inner.wakeup_state.compare_and_swap(curr, tick as usize, Ordering::Release);

                if actual == curr {
                    // Signal to the wakeup thread that the wakeup time has
                    // been changed.
                    inner.wakeup_thread.thread().unpark();
                    return;
                }

                curr = actual;
            }
        }
    }

    // Next tick containing a timeout
    fn next_tick(&self) -> Option<Tick> {
        if self.next != EMPTY {
            let slot = self.slot_for(self.entries[self.next].links.tick);

            if self.wheel[slot].next_tick == self.tick {
                // There is data ready right now
                return Some(self.tick);
            }
        }

        self.wheel.iter().map(|e| e.next_tick).min()
    }

    fn current_tick(&self) -> Tick {
        self.ms_to_tick(now_ms())
    }

    // Convert a ms duration into a number of ticks, rounds up
    fn ms_to_tick(&self, ms: u64) -> Tick {
        ms_to_tick(ms, self.start, self.tick_ms)
    }

    fn slot_for(&self, tick: Tick) -> usize {
        (self.mask & tick) as usize
    }
}

impl<T> Default for Timer<T> {
    fn default() -> Timer<T> {
        Builder::default().build()
    }
}

impl<T> Evented for Timer<T> {
    fn register(&self, poll: &Poll, token: Token, interest: EventSet, opts: PollOpt) -> io::Result<()> {
        if self.inner.is_some() {
            return Err(io::Error::new(io::ErrorKind::Other, "timer already registered"));
        }

        let (registration, set_readiness) = Registration::new(poll, token, interest, opts);
        let wakeup_state = Arc::new(AtomicUsize::new(usize::MAX));
        let thread_handle = spawn_wakeup_thread(
            wakeup_state.clone(),
            set_readiness.clone(),
            self.start, self.tick_ms);

        self.inner.set(Inner {
            registration: registration,
            set_readiness: set_readiness,
            wakeup_state: wakeup_state,
            wakeup_thread: thread_handle,
        }).ok().expect("timer already registered");

        if let Some(next_tick) = self.next_tick() {
            self.schedule_readiness(next_tick);
        }

        Ok(())
    }

    fn reregister(&self, poll: &Poll, token: Token, interest: EventSet, opts: PollOpt) -> io::Result<()> {
        match self.inner.as_ref() {
            Some(inner) => inner.registration.update(poll, token, interest, opts),
            None => Err(io::Error::new(io::ErrorKind::Other, "receiver not registered")),
        }
    }

    fn deregister(&self, poll: &Poll) -> io::Result<()> {
        match self.inner.as_ref() {
            Some(inner) => inner.registration.deregister(poll),
            None => Err(io::Error::new(io::ErrorKind::Other, "receiver not registered")),
        }
    }
}

impl fmt::Debug for Inner {
    fn fmt(&self, fmt: &mut fmt::Formatter) -> fmt::Result {
        fmt.debug_struct("Inner")
            .field("registration", &self.registration)
            .field("wakeup_state", &self.wakeup_state.load(Ordering::Relaxed))
            .finish()
    }
}

fn spawn_wakeup_thread(state: WakeupState, set_readiness: SetReadiness, ticks_start_at: u64, tick_ms: u64) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        let mut sleep_until_tick = state.load(Ordering::Acquire) as Tick;

        loop {
            if sleep_until_tick == TERMINATE_THREAD as Tick {
                return;
            }

            let now_tick = now_tick(ticks_start_at, tick_ms);

            trace!("wakeup thread: sleep_until_tick={:?}; now_tick={:?}", sleep_until_tick, now_tick);

            if now_tick < sleep_until_tick {
                let sleep_duration = tick_ms.checked_mul(sleep_until_tick - now_tick).unwrap_or(u64::MAX);
                trace!("sleeping; tick_ms={}; now_tick={}; sleep_until_tick={}; duration={:?}", tick_ms, now_tick, sleep_until_tick, sleep_duration);
                thread::park_timeout(Duration::from_millis(sleep_duration));
                sleep_until_tick = state.load(Ordering::Acquire) as Tick;
            } else {
                let actual = state.compare_and_swap(sleep_until_tick as usize, usize::MAX, Ordering::AcqRel) as Tick;

                if actual == sleep_until_tick {
                    trace!("setting readiness from wakeup thread");
                    let _ = set_readiness.set_readiness(EventSet::readable());
                    sleep_until_tick = usize::MAX as Tick;
                } else {
                    sleep_until_tick = actual as Tick;
                }
            }
        }
    })
}

fn now_ms() -> u64 {
    let now = match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(dur) => dur,
        Err(err) => err.duration(),
    };
    now.as_secs() * 1000 + (now.subsec_nanos() / NS_PER_MS) as u64
}

fn now_tick(start: u64, tick_ms: u64) -> Tick {
    ms_to_tick(now_ms(), start, tick_ms)
}

fn ms_to_tick(ms: u64, start: u64, tick_ms: u64) -> Tick {
    (ms - start) / tick_ms
}

impl<T> Entry<T> {
    fn new(state: T, tick: u64, next: Token) -> Entry<T> {
        Entry {
            state: state,
            links: EntryLinks {
                tick: tick,
                prev: EMPTY,
                next: next,
            },
        }
    }
}

impl fmt::Display for TimerError {
    fn fmt(&self, fmt: &mut fmt::Formatter) -> fmt::Result {
        write!(fmt, "{}: {}", self.kind, self.desc)
    }
}

impl TimerError {
    fn overflow() -> TimerError {
        TimerError {
            kind: TimerOverflow,
            desc: "too many timer entries"
        }
    }
}

impl error::Error for TimerError {
    fn description(&self) -> &str {
        self.desc
    }
}

impl fmt::Display for TimerErrorKind {
    fn fmt(&self, fmt: &mut fmt::Formatter) -> fmt::Result {
        match *self {
            TimerOverflow => write!(fmt, "TimerOverflow"),
        }
    }
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    pub fn test_timeout_next_tick() {
        let mut t = timer();
        let mut tick;

        t.set_timeout_at(100, "a").unwrap();

        tick = t.ms_to_tick(50);
        assert_eq!(None, t.poll_to(tick));

        tick = t.ms_to_tick(100);
        assert_eq!(Some("a"), t.poll_to(tick));
        assert_eq!(None, t.poll_to(tick));

        tick = t.ms_to_tick(150);
        assert_eq!(None, t.poll_to(tick));

        tick = t.ms_to_tick(200);
        assert_eq!(None, t.poll_to(tick));

        assert_eq!(count(&t), 0);
    }

    #[test]
    pub fn test_clearing_timeout() {
        let mut t = timer();
        let mut tick;

        let to = t.set_timeout_at(100, "a").unwrap();
        assert_eq!("a", t.cancel_timeout(&to).unwrap());

        tick = t.ms_to_tick(100);
        assert_eq!(None, t.poll_to(tick));

        tick = t.ms_to_tick(200);
        assert_eq!(None, t.poll_to(tick));

        assert_eq!(count(&t), 0);
    }

    #[test]
    pub fn test_multiple_timeouts_same_tick() {
        let mut t = timer();
        let mut tick;

        t.set_timeout_at(100, "a").unwrap();
        t.set_timeout_at(100, "b").unwrap();

        let mut rcv = vec![];

        tick = t.ms_to_tick(100);
        rcv.push(t.poll_to(tick).unwrap());
        rcv.push(t.poll_to(tick).unwrap());

        assert_eq!(None, t.poll_to(tick));

        rcv.sort();
        assert!(rcv == ["a", "b"], "actual={:?}", rcv);

        tick = t.ms_to_tick(200);
        assert_eq!(None, t.poll_to(tick));

        assert_eq!(count(&t), 0);
    }

    #[test]
    pub fn test_multiple_timeouts_diff_tick() {
        let mut t = timer();
        let mut tick;

        t.set_timeout_at(110, "a").unwrap();
        t.set_timeout_at(220, "b").unwrap();
        t.set_timeout_at(230, "c").unwrap();
        t.set_timeout_at(440, "d").unwrap();
        t.set_timeout_at(560, "e").unwrap();

        tick = t.ms_to_tick(100);
        assert_eq!(Some("a"), t.poll_to(tick));
        assert_eq!(None, t.poll_to(tick));

        tick = t.ms_to_tick(200);
        assert_eq!(Some("c"), t.poll_to(tick));
        assert_eq!(Some("b"), t.poll_to(tick));
        assert_eq!(None, t.poll_to(tick));

        tick = t.ms_to_tick(300);
        assert_eq!(None, t.poll_to(tick));

        tick = t.ms_to_tick(400);
        assert_eq!(Some("d"), t.poll_to(tick));
        assert_eq!(None, t.poll_to(tick));

        tick = t.ms_to_tick(500);
        assert_eq!(None, t.poll_to(tick));

        tick = t.ms_to_tick(600);
        assert_eq!(Some("e"), t.poll_to(tick));
        assert_eq!(None, t.poll_to(tick));
    }

    #[test]
    pub fn test_catching_up() {
        let mut t = timer();

        t.set_timeout_at(110, "a").unwrap();
        t.set_timeout_at(220, "b").unwrap();
        t.set_timeout_at(230, "c").unwrap();
        t.set_timeout_at(440, "d").unwrap();

        let tick = t.ms_to_tick(600);
        assert_eq!(Some("a"), t.poll_to(tick));
        assert_eq!(Some("c"), t.poll_to(tick));
        assert_eq!(Some("b"), t.poll_to(tick));
        assert_eq!(Some("d"), t.poll_to(tick));
        assert_eq!(None, t.poll_to(tick));
    }

    #[test]
    pub fn test_timeout_hash_collision() {
        let mut t = timer();
        let mut tick;

        t.set_timeout_at(100, "a").unwrap();
        t.set_timeout_at(100 + TICK * SLOTS as u64, "b").unwrap();

        tick = t.ms_to_tick(100);
        assert_eq!(Some("a"), t.poll_to(tick));
        assert_eq!(1, count(&t));

        tick = t.ms_to_tick(200);
        assert_eq!(None, t.poll_to(tick));
        assert_eq!(1, count(&t));

        tick = t.ms_to_tick(100 + TICK * SLOTS as u64);
        assert_eq!(Some("b"), t.poll_to(tick));
        assert_eq!(0, count(&t));
    }

    #[test]
    pub fn test_clearing_timeout_between_triggers() {
        let mut t = timer();
        let mut tick;

        let a = t.set_timeout_at(100, "a").unwrap();
        let _ = t.set_timeout_at(100, "b").unwrap();
        let _ = t.set_timeout_at(200, "c").unwrap();

        tick = t.ms_to_tick(100);
        assert_eq!(Some("b"), t.poll_to(tick));
        assert_eq!(2, count(&t));

        t.cancel_timeout(&a);
        assert_eq!(1, count(&t));

        assert_eq!(None, t.poll_to(tick));

        tick = t.ms_to_tick(200);
        assert_eq!(Some("c"), t.poll_to(tick));
        assert_eq!(0, count(&t));
    }

    const TICK: u64 = 100;
    const SLOTS: usize = 16;
    const CAPACITY: usize = 32;

    fn count<T>(timer: &Timer<T>) -> usize {
        timer.entries.count()
    }

    fn timer() -> Timer<&'static str> {
        Timer::new(TICK, SLOTS, CAPACITY, 0)
    }
}
