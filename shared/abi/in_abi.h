#ifndef IN_ABI_H
#define IN_ABI_H

#include <stdint.h>

#define IN_ABI_VERSION 1

typedef enum InAbiStatus {
  IN_ABI_OK = 0,
  IN_ABI_PANIC = 1,
  IN_ABI_LAYOUT_MISMATCH = 2,
  IN_ABI_ALLOC_ERROR = 3,
  IN_ABI_SYMBOL_MISSING = 4
} InAbiStatus;

typedef struct InCallStatus {
  uint32_t code;
  uint32_t reserved;
  uint64_t error_len;
  const uint8_t *error_ptr;
} InCallStatus;

typedef struct InSliceU8 {
  const uint8_t *ptr;
  uint64_t len;
} InSliceU8;

typedef struct InBufU8 {
  uint8_t *ptr;
  uint64_t len;
  uint64_t cap;
  uint64_t allocator_id;
} InBufU8;

typedef struct InArenaHandle {
  uint64_t id;
  uint64_t generation;
} InArenaHandle;

typedef struct InBorrowToken {
  uint64_t arena_id;
  uint64_t generation;
  uint64_t start;
  uint64_t len;
  uint64_t flags;
} InBorrowToken;

struct InHostVTable;
struct InAbiManifest;

typedef struct InModuleVTable {
  uint32_t abi_version;
  uint32_t pointer_width;
  uint32_t endian;
  uint32_t layout_hash;
  void *(*alloc)(uint64_t size, uint64_t align, uint64_t arena_id);
  void (*dealloc)(void *ptr, uint64_t size, uint64_t align, uint64_t allocator_id);
  InCallStatus (*init)(const struct InHostVTable *host);
  InCallStatus (*shutdown)(void);
  const void *(*symbol)(const char *name, uint64_t name_len);
  const struct InAbiManifest *(*manifest)(void);
} InModuleVTable;

InArenaHandle in_arena_create(uint64_t reserve_bytes, uint32_t flags);
void in_arena_reset(InArenaHandle arena);
void in_arena_destroy(InArenaHandle arena);

// align is a power of two (0 means 1); invalid requests return a null, empty buffer.
InBufU8 in_buf_from_host_arena(InArenaHandle arena, uint64_t len, uint64_t align);
InSliceU8 in_borrow_bytes(InBufU8 *buf, InBorrowToken *out_token);
uint32_t in_borrow_validate(InBorrowToken token);

#endif