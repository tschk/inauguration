use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct InArenaHandle {
    pub id: u64,
    pub generation: u64,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InBufU8 {
    pub ptr: *mut u8,
    pub len: u64,
    pub cap: u64,
    pub allocator_id: u64,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InSliceU8 {
    pub ptr: *const u8,
    pub len: u64,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InBorrowToken {
    pub arena_id: u64,
    pub generation: u64,
    pub start: u64,
    pub len: u64,
    pub flags: u64,
}

struct ArenaState {
    generation: u64,
    borrow_epoch: u64,
    storage: Vec<u8>,
    cursor: usize,
}

static NEXT_ARENA_ID: AtomicU64 = AtomicU64::new(1);
static ARENAS: OnceLock<Mutex<HashMap<u64, ArenaState>>> = OnceLock::new();

fn arenas() -> &'static Mutex<HashMap<u64, ArenaState>> {
    ARENAS.get_or_init(|| Mutex::new(HashMap::new()))
}

pub fn in_arena_create(reserve_bytes: u64, _flags: u32) -> InArenaHandle {
    let id = NEXT_ARENA_ID.fetch_add(1, Ordering::SeqCst);
    let reserve = reserve_bytes.max(64) as usize;
    let state = ArenaState {
        generation: 1,
        borrow_epoch: 1,
        storage: vec![0; reserve],
        cursor: 0,
    };
    arenas().lock().expect("arena map lock").insert(id, state);
    InArenaHandle { id, generation: 1 }
}

pub fn in_arena_reset(arena: InArenaHandle) {
    let mut map = arenas().lock().expect("arena map lock");
    if let Some(state) = map.get_mut(&arena.id) {
        if state.generation != arena.generation {
            return;
        }
        let Some(epoch) = state.borrow_epoch.checked_add(1) else {
            return;
        };
        state.borrow_epoch = epoch;
        state.cursor = 0;
    }
}

pub fn in_arena_destroy(arena: InArenaHandle) {
    let mut map = arenas().lock().expect("arena map lock");
    if let Some(state) = map.get(&arena.id)
        && state.generation == arena.generation
    {
        map.remove(&arena.id);
    }
}

pub fn in_buf_from_host_arena(arena: InArenaHandle, len: u64, align: u64) -> InBufU8 {
    let align = align.max(1) as usize;
    let len = len as usize;
    let mut map = arenas().lock().expect("arena map lock");
    let Some(state) = map.get_mut(&arena.id) else {
        return InBufU8 {
            ptr: std::ptr::null_mut(),
            len: 0,
            cap: 0,
            allocator_id: arena.id,
        };
    };
    if state.generation != arena.generation {
        return InBufU8 {
            ptr: std::ptr::null_mut(),
            len: 0,
            cap: 0,
            allocator_id: arena.id,
        };
    }
    let aligned = align_up(state.cursor, align);
    let end = aligned.saturating_add(len);
    if end > state.storage.len() {
        let Some(epoch) = state.borrow_epoch.checked_add(1) else {
            return InBufU8 {
                ptr: std::ptr::null_mut(),
                len: 0,
                cap: 0,
                allocator_id: arena.id,
            };
        };
        let reallocates = end > state.storage.capacity();
        state.storage.resize(end, 0);
        if reallocates {
            state.borrow_epoch = epoch;
        }
    }
    state.cursor = end;
    let ptr = unsafe { state.storage.as_mut_ptr().add(aligned) };
    InBufU8 {
        ptr,
        len: len as u64,
        cap: len as u64,
        allocator_id: arena.id,
    }
}

// shortcut: tokens detect expired views but do not pin storage against concurrent mutation; add borrow guards before exposing safe dereferencing.
pub fn in_borrow_bytes(buf: &mut InBufU8, out_token: &mut InBorrowToken) -> InSliceU8 {
    *out_token = InBorrowToken {
        arena_id: buf.allocator_id,
        generation: 0,
        start: 0,
        len: 0,
        flags: 0,
    };
    let map = arenas().lock().expect("arena map lock");
    let valid = map.get(&buf.allocator_id).and_then(|state| {
        let start = (buf.ptr as usize).checked_sub(state.storage.as_ptr() as usize)?;
        let len = usize::try_from(buf.len).ok()?;
        let end = start.checked_add(len)?;
        (end <= state.cursor && buf.len <= buf.cap).then_some((state.borrow_epoch, start))
    });
    let Some((epoch, start)) = valid else {
        return InSliceU8 {
            ptr: std::ptr::null(),
            len: 0,
        };
    };
    out_token.generation = epoch;
    out_token.start = start as u64;
    out_token.len = buf.len;
    InSliceU8 {
        ptr: buf.ptr,
        len: buf.len,
    }
}

pub fn in_borrow_validate(token: InBorrowToken) -> u32 {
    let map = arenas().lock().expect("arena map lock");
    let Some(state) = map.get(&token.arena_id) else {
        return 2;
    };
    if token.generation != state.borrow_epoch {
        return 2;
    }
    let Some(end) = token.start.checked_add(token.len) else {
        return 2;
    };
    if end > state.cursor as u64 {
        return 2;
    }
    if token.len == 0 {
        return 1;
    }
    0
}

fn align_up(value: usize, align: usize) -> usize {
    let mask = align.saturating_sub(1);
    (value + mask) & !mask
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn arena_alloc_and_reset() {
        let arena = in_arena_create(128, 0);
        let buf = in_buf_from_host_arena(arena, 16, 8);
        assert!(!buf.ptr.is_null());
        assert_eq!(buf.len, 16);
        in_arena_reset(arena);
        let buf2 = in_buf_from_host_arena(arena, 8, 8);
        assert!(!buf2.ptr.is_null());
        in_arena_destroy(arena);
    }

    #[test]
    fn borrow_tokens_track_offsets_and_reset_epochs() {
        let arena = in_arena_create(64, 0);
        let _first = in_buf_from_host_arena(arena, 8, 8);
        let mut second = in_buf_from_host_arena(arena, 8, 8);
        let mut token = InBorrowToken {
            arena_id: 0,
            generation: 0,
            start: 0,
            len: 0,
            flags: 0,
        };
        assert_eq!(in_borrow_bytes(&mut second, &mut token).len, 8);
        assert_eq!(token.start, 8);
        assert_ne!(token.generation, 0);
        assert_eq!(in_borrow_validate(token), 0);
        assert_eq!(
            in_borrow_validate(InBorrowToken {
                start: u64::MAX,
                ..token
            }),
            2
        );
        assert_eq!(
            in_borrow_validate(InBorrowToken {
                start: 16,
                len: 1,
                ..token
            }),
            2
        );
        in_arena_reset(arena);
        assert_eq!(in_borrow_validate(token), 2);
        let mut current = in_buf_from_host_arena(arena, 8, 8);
        assert_eq!(in_borrow_bytes(&mut current, &mut token).len, 8);
        assert_eq!(token.start, 0);
        assert_eq!(in_borrow_validate(token), 0);
        in_arena_destroy(arena);
    }

    #[test]
    fn storage_growth_expires_existing_borrow_tokens() {
        let arena = in_arena_create(64, 0);
        let mut buf = in_buf_from_host_arena(arena, 8, 8);
        let mut token = InBorrowToken {
            arena_id: 0,
            generation: 0,
            start: 0,
            len: 0,
            flags: 0,
        };
        in_borrow_bytes(&mut buf, &mut token);
        let capacity = arenas()
            .lock()
            .unwrap()
            .get(&arena.id)
            .unwrap()
            .storage
            .capacity();
        let mut grown = in_buf_from_host_arena(arena, capacity as u64 + 1, 8);
        assert_eq!(in_borrow_validate(token), 2);
        in_borrow_bytes(&mut grown, &mut token);
        assert_eq!(in_borrow_validate(token), 0);
        in_arena_destroy(arena);
    }

    #[test]
    fn invalid_buffers_and_expired_empty_tokens_are_refused() {
        let arena = in_arena_create(64, 0);
        let mut buf = in_buf_from_host_arena(arena, 0, 8);
        let mut token = InBorrowToken {
            arena_id: 0,
            generation: 0,
            start: 0,
            len: 0,
            flags: 0,
        };
        in_borrow_bytes(&mut buf, &mut token);
        assert_eq!(in_borrow_validate(token), 1);
        in_arena_reset(arena);
        assert_eq!(in_borrow_validate(token), 2);
        for mut invalid in [
            InBufU8 {
                ptr: std::ptr::null_mut(),
                len: 1,
                cap: 1,
                allocator_id: arena.id,
            },
            InBufU8 {
                ptr: buf.ptr,
                len: u64::MAX,
                cap: u64::MAX,
                allocator_id: arena.id,
            },
            InBufU8 {
                ptr: buf.ptr,
                len: 1,
                cap: 0,
                allocator_id: arena.id,
            },
        ] {
            let slice = in_borrow_bytes(&mut invalid, &mut token);
            assert!(slice.ptr.is_null());
            assert_eq!(slice.len, 0);
            assert_eq!(in_borrow_validate(token), 2);
        }
        in_arena_destroy(arena);
    }

    #[test]
    fn borrow_token_validates_live_slice() {
        let arena = in_arena_create(64, 0);
        let mut buf = in_buf_from_host_arena(arena, 8, 8);
        let mut token = InBorrowToken {
            arena_id: 0,
            generation: 0,
            start: 0,
            len: 0,
            flags: 0,
        };
        let slice = in_borrow_bytes(&mut buf, &mut token);
        assert_eq!(slice.len, 8);
        assert_eq!(in_borrow_validate(token), 0);
        in_arena_destroy(arena);
        assert_eq!(in_borrow_validate(token), 2);
    }
}
