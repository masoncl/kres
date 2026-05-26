---
name: systemd
description: Load anytime the working directory is a systemd tree. Consolidated systemd-specific technical knowledge — conventions, subsystem patterns, common pitfalls.
invocation_policy: automatic
---

# systemd Technical Knowledge

This skill is a single consolidated reference of systemd-specific technical
knowledge: conventions, subsystem patterns, sequencing rules, and common
pitfalls. It contains knowledge only — no review protocols, debugging
protocols, or output-format machinery.

## Semcode Integration

When available, semcode MCP tools are the preferred way to navigate the
systemd codebase:

- `find_function` / `find_type` — get function and type definitions
- `find_callchain` — trace call relationships up and down
- `find_callers` / `find_calls` — explore call graphs
- `grep_functions` — search function bodies with regex
- `diff_functions` — identify changed functions in patches
- `find_commit` / `vcommit_similar_commits` — search commit history

The [semcode repository](https://github.com/facebookexperimental/semcode)
has setup instructions.

## Build Commands

For systemd Meson builds, prefer:

```sh
meson compile -C build
```

For cleaning, use Meson's compile clean mode, not a top-level `meson clean`
command:

```sh
meson compile -C build --clean
```

`ninja -C build clean` is also valid. `meson clean -C build` is not valid for
the Meson version used here because `clean` is not a top-level Meson subcommand.

---

# Core Conventions

## Error Handling

**Return codes**
- Errors are returned as negative `Exxx`, e.g. `return -EINVAL;`
- For constructors, returning `NULL` on OOM is acceptable
- For lookup functions, `NULL` is acceptable for "not found"
- Use `RET_NERRNO()` to convert libc style (-1/errno) to systemd style (-errno)

**Logging rules**
- "Library" code (`src/basic/`, `src/shared/`) must NOT log (except DEBUG level)
- "Main program" code does logging
- "Logging" functions should not log errors from other "logging" functions
- Use `log_error_errno(r, "message: %m")` for combined log and return
- Use `SYNTHETIC_ERRNO(E...)` when the error is not from a called function

**Assert usage**
- `assert_return()` — for public API parameter validation, returns error code
- `assert()` — for internal programming-error detection, aborts
- Both are for programming errors only, not runtime errors

**Ignoring errors**
- Cast to `(void)` when intentionally ignoring return values:
  `(void) unlink("/foo/bar/baz");`

## Memory Management

**Cleanup attributes**
- `_cleanup_free_` — auto-free with `free()`
- `_cleanup_close_` — auto-close file descriptors
- `_cleanup_fclose_` — auto-close `FILE*`
- `_cleanup_(foo_freep)` — custom cleanup with `foo_freep`

**Ownership transfer**
- `TAKE_PTR(p)` — transfer pointer ownership, sets `p` to `NULL`
- `TAKE_FD(fd)` — transfer fd ownership, sets `fd` to `-EBADF`

**Allocation rules**
- Always check OOM — no exceptions
- Use `log_oom()` in program code (not library code)
- Avoid fixed-size string buffers unless maximum is known and small
- Never use `alloca()` directly — use `alloca_safe()`, `strdupa_safe()`
- Never use `alloca_safe()` in loops or function parameters

## File Descriptors

**O_CLOEXEC requirement**
- ALL file descriptors must be `O_CLOEXEC` from creation
- `open()` must include `O_CLOEXEC`
- `socket()`/`socketpair()` must include `SOCK_CLOEXEC`
- `recvmsg()` must include `MSG_CMSG_CLOEXEC`
- Use `F_DUPFD_CLOEXEC` instead of `F_DUPFD`
- `fopen()` should use the `"e"` flag

**Other FD rules**
- Never use `dup()` — use `fcntl(fd, F_DUPFD_CLOEXEC, 3)`
- The `3` avoids stdin/stdout/stderr (0, 1, 2)
- Use `O_NONBLOCK` when opening 'foreign' regular files
- Use `safe_close()` which handles `-EBADF`
- Initialize cleanup FDs to `-EBADF`

## Threading

**No threads in PID1 — critical**
- PID1 must NEVER use threads
- Cannot mix `malloc` in threads with `clone()`/`clone3()` syscalls
- Risk of deadlock: child inherits locked malloc mutex
- Fork worker processes instead of worker threads
- Use `posix_spawn()` which combines `clone()` + `execve()`
- `fork()` synchronizes malloc locks; `clone()` does not

**Thread safety**
- Library code should be thread-safe
- Use TLS (`thread_local`) for per-thread caching
- Use `is_main_thread()` to detect the main thread
- Disable caching in non-main threads

## NSS and Deadlock Prevention

**No NSS from PID1**
- Never issue NSS requests (user/hostname lookups) from PID1
- NSS may synchronously talk to services we need to start
- Risk of deadlock

**No synchronous IPC from PID1**
- Do not synchronously talk to any service from PID1
- Risk of deadlocks

## Coding Style

**Naming**
- Structures: `PascalCase` (except public API)
- Variables and functions: `snake_case`
- Return parameters: prefix with `ret_` (success) or `reterr_` (failure)
- Command-line variables: prefix with `arg_`

**Destructor patterns**
- Destructors must accept `NULL` and treat it as a NOP (like `free()`)
- Destructors should return the same type and always return `NULL`
- Enables the pattern: `p = foobar_unref(p);`
- Destructors deregister from a larger object, not vice versa
- Destructors must handle half-initialized objects

**Destructor naming**
- `xyz_free()` — full destruction, frees all memory
- `xyz_done()` — destroys content, leaves object allocated
- `xyz_clear()` — like `done()`, but resets for reuse
- `xyz_unref()` — decrement refcount
- `xyz_ref()` — increment refcount

## Type Safety

**Preferred types**
- Use `unsigned` not `unsigned int`
- Use `char` only for characters, `uint8_t` for bytes
- Never use `short` types
- Never use `off_t` — use `uint64_t`
- Use `bool` internally, `int` in public APIs (C89 compat)
- Use `double` over `float` (unless array allocation)

**Time values**
- Always use `usec_t` for time values
- Don't mix usec/msec

## Functions to Avoid

| Bad | Use instead |
|-----|-------------|
| `memset(..., 0, ...)` | `memzero()` or `zero()` |
| `strcmp()` for equality | `streq()` |
| `strtol()` / `atoi()` | `safe_atoli()`, `safe_atou32()` |
| `htonl()` / `ntohl()` | `htobe32()`, `htobe16()` |
| `inet_ntop()` | `IN_ADDR_TO_STRING()` macros |
| `dup()` | `fcntl(fd, F_DUPFD_CLOEXEC, 3)` |
| `fgets()` | `read_line()` |
| `exit()` | propagate errors up; `_exit()` in forked children |
| `basename()` / `dirname()` | `path_extract_filename()` / `path_extract_directory()` |
| `FILENAME_MAX` | `PATH_MAX` or `NAME_MAX` |

## Control Flow

**goto**
- Only use `goto` for cleanup
- Only jump to end of function, never backwards

**Loops**
- Use `for (;;)` for infinite loops, not `while (1)`

---

# Cleanup Attribute Patterns

## Common Cleanup Attributes

| Macro | Calls | Safe for NULL? |
|-------|-------|----------------|
| `_cleanup_free_` | `free()` | Yes |
| `_cleanup_close_` | `safe_close()` | Yes (-EBADF) |
| `_cleanup_fclose_` | `safe_fclose()` | Yes |
| `_cleanup_closedir_` | `safe_closedir()` | Yes |
| `_cleanup_hashmap_free_` | `hashmap_free()` | Yes |
| `_cleanup_set_free_` | `set_free()` | Yes |
| `_cleanup_strv_free_` | `strv_free()` | Yes |
| `_cleanup_(sd_bus_unrefp)` | `sd_bus_unref()` | Yes |
| `_cleanup_(sd_event_unrefp)` | `sd_event_unref()` | Yes |

## Ownership Transfer

```c
/* Transfer pointer ownership */
result = TAKE_PTR(p);  /* p becomes NULL */

/* Transfer FD ownership */
result_fd = TAKE_FD(fd);  /* fd becomes -EBADF */
```

## Cleanup Function Compatibility

Cleanup functions must handle every value the variable can hold:
- `NULL` (most cleanups handle this)
- Partially initialized objects
- Error values (some allocators may leave the variable holding `-ERRNO` in FDs)

Correct initialization:

```c
_cleanup_free_ char *p = NULL;
p = strdup("hello");
if (!p)
        return -ENOMEM;  /* free(NULL) is safe */
```

```c
_cleanup_close_ int fd = -EBADF;  /* Initialize to invalid */
fd = open(path, O_RDONLY);
if (fd < 0)
        return -errno;  /* safe_close(-EBADF) is safe */
```

## LIFO Cleanup Order

Cleanup runs in **reverse definition order** (last defined = first cleaned).

```c
_cleanup_close_ int fd = -EBADF;  /* Defined first, cleaned last */
_cleanup_free_ char *buf = NULL;  /* Defined second, cleaned first */

fd = open(path, O_RDONLY);
buf = malloc(SIZE);
/* On return: free(buf) then safe_close(fd) */
```

Critical case with locks/guards — define the lock *before* the resource it
protects, so the resource is freed under the lock:

```c
/* WRONG — resource defined before lock; lock released before free */
_cleanup_free_ Object *obj = NULL;
_cleanup_(mutex_unlockp) Mutex *m = mutex_lock(&lock);
obj = allocate_under_lock();

/* CORRECT — lock first, freed last */
_cleanup_(mutex_unlockp) Mutex *m = mutex_lock(&lock);
_cleanup_free_ Object *obj = allocate_under_lock();
```

## Do Not Mix goto and Cleanup Attributes

In a single function, use either the goto-cleanup pattern OR cleanup
attributes — never both.

```c
/* WRONG */
_cleanup_free_ char *a = strdup("a");
char *b = strdup("b");
if (!a || !b)
        goto cleanup;
cleanup:
        free(b);

/* CORRECT */
_cleanup_free_ char *a = strdup("a");
_cleanup_free_ char *b = strdup("b");
if (!a || !b)
        return -ENOMEM;
```

## When to Use TAKE_PTR / TAKE_FD

- Passing ownership to another structure
- Returning ownership to the caller
- Storing in a container that will free it

```c
_cleanup_free_ char *p = strdup("hello");
if (!p)
        return -ENOMEM;

r = hashmap_put(h, key, p);
if (r < 0)
        return r;  /* p still owned, will be freed */

TAKE_PTR(p);  /* Hash table owns it now */
return 0;
```

```c
_cleanup_close_ int fd = open(path, O_RDONLY|O_CLOEXEC);
if (fd < 0)
        return -errno;

*ret_fd = TAKE_FD(fd);  /* fd becomes -EBADF */
return 0;
```

---

# Service Manager (PID1)

## Key Files

- `src/core/manager.c` — main manager loop and state
- `src/core/unit.c` — unit lifecycle
- `src/core/service.c` — service unit implementation
- `src/core/execute.c` — execution context
- `src/core/exec-invoke.c` — process spawning and sandboxing
- `src/core/load-fragment.c` — unit file parsing
- `src/core/dbus-*.c` — D-Bus interfaces

## Critical PID1 Rules

- **No threading in PID1.** Cannot mix threads with `clone()`/`clone3()` due
  to the malloc lock. `fork()` synchronizes malloc locks; `clone()` does not.
  Use forking processes instead.
- **No NSS calls from PID1.** NSS lookups (user/host names) may trigger
  service starts, creating a circular dependency and deadlock.
- **No synchronous service calls from PID1.** PID1 must never wait
  synchronously for service responses; all service communication is async.

## Unit Lifecycle

State machine:

```
UNIT_STUB -> UNIT_LOADED -> active states -> UNIT_FAILED / UNIT_INACTIVE
```

Reference counting:

```c
Unit *u;
u = unit_ref(existing_unit);  /* Increment reference */
/* ... use u ... */
unit_unref(u);                /* Decrement reference */
```

Every `unit_ref()` must have a matching `unit_unref()`. References must be
held across asynchronous operations.

Jobs represent pending state changes and must be properly linked/unlinked
from their unit; job completion must trigger the correct callbacks; error
paths must not leak jobs.

## Execution Context

PID1 serializes execution context, then `systemd-executor` deserializes it
and execs:

```
PID1 -> memfd -> systemd-executor -> exec
```

```c
/* In PID1 */
exec_serialize(context, ...);

/* In executor */
exec_deserialize(context, ...);
```

Rules:
- All fields needed by the executor must be serialized
- Deserialization must handle missing or malformed data
- Never serialize pointers — only values

## Sandbox Application Order

Inside `systemd-executor`, sandboxing must be applied in this order:

1. User/group switching
2. Namespace setup
3. Seccomp filters (last, because they restrict syscalls)

Namespace setup must precede mounts; capabilities must be dropped at the
right point relative to operations that require them.

## D-Bus Integration in PID1

Object lifecycle:

```c
r = bus_unit_implement(u);     /* Register */
/* Object must outlive any D-Bus references */
bus_unit_unimplement(u);       /* Unregister before freeing */
```

Async callbacks must check object validity. Errors are returned via
`sd_bus_error` replies.

## Memory Safety in PID1

Hash-table iteration: don't modify the hashmap while iterating with
`HASHMAP_FOREACH`. Mark entries for later removal, or use
`hashmap_foreach_remove()`.

```c
HASHMAP_FOREACH(u, m->units) {
        if (should_remove(u))
                /* mark for later removal */;
}
```

Event sources:

```c
_cleanup_(sd_event_source_unrefp) sd_event_source *s = NULL;
r = sd_event_add_io(e, &s, fd, EPOLLIN, callback, userdata);
```

Sources must be disabled or unrefd before associated data is freed.
Callbacks must consider that destruction may be in progress.

## Configuration Parsing

A new unit-file setting requires three interfaces to be implemented:

1. `src/core/load-fragment.c` — INI file parsing
2. `src/core/dbus-*.c` — D-Bus property
3. `src/shared/bus-unit-util.c` — `systemctl` interface

A new setting also requires updates to the fuzzer corpus under
`test/fuzz/fuzz-unit-file/` and man-page documentation.

## Resilience

PID1 must not crash during boot:
- OOM is handled gracefully (emergency mode)
- Missing files must not crash
- Invalid configuration must not crash

`daemon-reload` must be safe and reversible:
- A failed reload must not corrupt state
- Resources must be properly cleaned up on reload
- Running services must not be affected by reload failures

---

# Namespaces

## Key Files

- `src/core/namespace.c` — main namespace setup (~4000 lines)
- `src/core/namespace.h` — types: `NamespaceParameters`, `BindMount`, etc.
- `src/basic/namespace-util.c` — low-level operations
- `src/core/exec-invoke.c` — `apply_mount_namespace()`
- `src/nspawn/nspawn-mount.c` — container mount handling

## Mount Namespace Creation Sequence

```c
/* 1. Create new mount namespace */
if (unshare(CLONE_NEWNS) < 0)
        return log_debug_errno(errno, "Failed to unshare mount namespace: %m");

/* 2. Isolate from parent (stop receiving propagation) */
if (mount(NULL, "/", NULL, MS_SLAVE|MS_REC, NULL) < 0)
        return log_debug_errno(errno, "Failed to remount '/' as SLAVE: %m");

/* 3. Apply mount entries (bind mounts, tmpfs, etc.) */
/* ... */

/* 4. Set final propagation mode */
if (mount(NULL, "/", NULL, mount_propagation_flag | MS_REC, NULL) < 0)
        return log_debug_errno(errno, "Failed to set propagation: %m");
```

`MS_SLAVE|MS_REC` must be applied before any other mounts.

## Namespace FD Handling

```c
fd = open("/proc/PID/ns/mnt", O_RDONLY|O_CLOEXEC);

if (setns(mntns_fd, CLONE_NEWNS) < 0)
        return -errno;
```

All namespace FDs require `O_CLOEXEC` and cleanup attributes. FDs must be
closed on every error path.

## Namespace Permission Checks

- `may_mount()` must be called before mount operations in a new namespace
- `ns_capable()` must be checked for cross-namespace operations
- User-namespace ownership must be verified for mount-namespace operations

## User Namespace + Mount Namespace Interaction

When creating a mount namespace in a user-namespace context, order matters:

```c
unshare(CLONE_NEWUSER);   /* User namespace first */
unshare(CLONE_NEWNS);     /* Mount namespace second, owned by new userns */
```

`CL_SLAVE` may be needed when crossing user-namespace boundaries; the mount
tree may need to be locked when the user namespace differs.

## Mount Propagation Flags

| Flag | Meaning |
|------|---------|
| `MS_SHARED` | Bidirectional propagation |
| `MS_SLAVE` | Receive from master, don't send back |
| `MS_PRIVATE` | No propagation |
| `MS_UNBINDABLE` | Can't be bind mounted |

`MS_SLAVE` is used for container isolation (receives updates, doesn't leak).
`MS_PRIVATE` is for complete isolation. `MS_SHARED` only when bidirectional
propagation is required.

## `detach_mount_namespace()`

Located in `src/basic/namespace-util.c:417`:

```c
int detach_mount_namespace(void) {
        if (unshare(CLONE_NEWNS) < 0)
                return log_debug_errno(...);

        if (mount(NULL, "/", NULL, MS_SLAVE|MS_REC, NULL) < 0)
                return log_debug_errno(...);

        if (mount(NULL, "/", NULL, MS_SHARED|MS_REC, NULL) < 0)
                return log_debug_errno(...);

        return 0;
}
```

Creates an isolated mount namespace that doesn't leak back.
`detach_mount_namespace_harder()` falls back to a user namespace when the
direct approach lacks privileges.

## Namespace Setup Ordering in the Executor

From `src/core/exec-invoke.c`:

1. Network namespace (if `PrivateNetwork=`)
2. IPC namespace (if `PrivateIPC=`)
3. Cgroup namespace (if delegated)
4. PID namespace (if `PrivatePIDs=`)
5. Mount namespace (if needed)
6. UTS namespace (if `ProtectHostname=`)

PID namespace must precede mount namespace so that `/proc` is mounted with
only processes from the PID namespace visible.

## Common Pitfalls

```c
/* BAD — error ignored */
unshare(CLONE_NEWNS);

/* GOOD */
if (unshare(CLONE_NEWNS) < 0)
        return log_debug_errno(errno, "...");
```

```c
/* BAD — MS_PRIVATE before MS_SLAVE leaks to parent first */
mount(NULL, "/", NULL, MS_PRIVATE|MS_REC, NULL);

/* GOOD — MS_SLAVE first stops propagation to parent */
mount(NULL, "/", NULL, MS_SLAVE|MS_REC, NULL);
```

```c
/* BAD — namespace FD leak on error */
mntns_fd = open("/proc/self/ns/mnt", O_RDONLY);
if (error_condition)
        return -ERRNO;

/* GOOD */
_cleanup_close_ int mntns_fd = -EBADF;
mntns_fd = open("/proc/self/ns/mnt", O_RDONLY|O_CLOEXEC);
```

---

# systemd-nspawn (Containers)

## Key Files

- `src/nspawn/nspawn.c` — main container logic
- `src/nspawn/nspawn-mount.c` — mount setup
- `src/nspawn/nspawn-mount.h` — mount types and flags
- `src/nspawn/nspawn-network.c` — network namespace setup
- `src/nspawn/nspawn-cgroup.c` — cgroup setup
- `src/nspawn/nspawn-seccomp.c` — seccomp filters

## CustomMount Settings

```c
typedef enum MountSettingsMask {
        MOUNT_FATAL              = 1 << 0,  /* Fail if mount fails */
        MOUNT_USE_USERNS         = 1 << 1,  /* Use user namespace */
        MOUNT_IN_USERNS          = 1 << 2,  /* Already in user namespace */
        MOUNT_APPLY_APIVFS_RO    = 1 << 3,  /* Apply read-only API VFS */
        MOUNT_APPLY_APIVFS_NETNS = 1 << 4,  /* Apply netns API VFS */
        /* ... */
} MountSettingsMask;
```

## Mount Order

1. Base filesystem mounts (rootfs)
2. API VFS mounts (`/proc`, `/sys`, `/dev`)
3. Custom bind mounts
4. Overlay mounts
5. Tmpfs mounts

Dependencies must be mounted first. Unmount order is the reverse.

## pivot_root

`src/nspawn/nspawn-mount.c`:

```c
int setup_pivot_root(const char *directory,
                     const char *pivot_root_new,
                     const char *pivot_root_old);
```

Requirements:
- The new root must be a mount point
- The old root must be unmountable after pivot
- The current directory must be handled correctly

If the new root isn't already a mount point, bind-mount it onto itself first:

```c
mount(new_root, new_root, NULL, MS_BIND, NULL);
pivot_root(new_root, put_old);
```

## Network Setup

```c
/* veth pair creation */
r = netlink_add_veth(host_ifname, container_ifname, ...);

/* Move interface to container namespace */
r = netlink_set_link_namespace(ifindex, netns_fd);
```

The host side of a veth pair stays in the host namespace; only the container
side moves. The host interface is configured after the container interface
has been moved. Partial failures must clean up.

```c
/* BAD — interface orphaned on error */
r = create_veth_pair(&host_if, &container_if);
if (r < 0)
        return r;
r = do_something_else();
if (r < 0)
        return r;  /* veth pair leaked */

/* GOOD — track for cleanup */
_cleanup_(destroy_veth) int host_if = -1;
r = create_veth_pair(&host_if, &container_if);
```

## UID/GID Mapping

Format of `/proc/PID/uid_map` and `/proc/PID/gid_map`:

```
container_id host_id count
```

Mappings must not overlap incorrectly. Root in the container must map to an
appropriate host UID. Consult `/etc/subuid` and `/etc/subgid` as needed.

## User Namespace + Mount Namespace

- Mount namespace inherits user-namespace ownership
- `CL_SLAVE` may be needed for mount copies
- Some mounts require `MS_BIND` together with user namespace
- Privilege checks must account for the user namespace

## Seccomp

Seccomp filters must be applied **last**, after all namespace and mount
setup is complete. Filters must not block syscalls that container setup
itself needs: `pivot_root`, `mount`/`umount`, `unshare`, `clone`.

## Container Teardown

On failure or exit:

1. Kill container processes
2. Unmount filesystems (reverse order)
3. Remove network interfaces
4. Clean up cgroup
5. Release namespace FDs

```c
_cleanup_(custom_mount_freep) CustomMount *m = NULL;

m = custom_mount_prepare(...);
if (m < 0)
        return m;

r = mount_custom(m, ...);
if (r < 0)
        return r;

/* Success — transfer ownership */
TAKE_PTR(m);
```

## Mount Propagation Pitfall

```c
/* BAD — mounts leak to host */
mount("/some/path", dest, ...);

/* GOOD — isolate first */
mount(NULL, "/", NULL, MS_SLAVE|MS_REC, NULL);
mount("/some/path", dest, ...);
```

---

# D-Bus (sd-bus)

## Key Files

- `src/libsystemd/sd-bus/` — core sd-bus implementation
- `src/shared/bus-util.c` — common bus utilities
- `src/shared/bus-unit-util.c` — unit-related bus operations
- `src/core/dbus-manager.c` — Manager D-Bus interface
- `src/core/dbus-unit.c` — Unit D-Bus interface

## Message Reads

```c
r = sd_bus_message_read(m, "s", &str);
if (r < 0)
        return sd_bus_error_set_errno(error, r);
```

The format string must match the message signature. Return values must
always be checked. Partial reads must be handled.

## Message Lifetime

Message data is only valid while the message is referenced:

```c
const char *str;
r = sd_bus_message_read(m, "s", &str);
/* str points into the message — don't store without copying */

/* CORRECT — copy if needed beyond the message lifetime */
_cleanup_free_ char *copy = strdup(str);
```

Pointers obtained from a message must not outlive the message reference.

## Array / Container Iteration

```c
r = sd_bus_message_enter_container(m, 'a', "s");
if (r < 0)
        return r;

while ((r = sd_bus_message_read(m, "s", &str)) > 0) {
        /* Process str */
}
if (r < 0)
        return r;

r = sd_bus_message_exit_container(m);
if (r < 0)
        return r;
```

Loop continues while `r > 0`. Containers must be entered before iteration
and exited after.

## Connection Lifecycle

```c
_cleanup_(sd_bus_flush_close_unrefp) sd_bus *bus = NULL;

r = sd_bus_open_system(&bus);
if (r < 0)
        return r;

/* Use bus... */
/* Automatic cleanup flushes, closes, unrefs */
```

## Slot / Callback Lifetime

```c
_cleanup_(sd_bus_slot_unrefp) sd_bus_slot *slot = NULL;

r = sd_bus_match_signal(bus, &slot, service, path, interface, member,
                        callback, userdata);
```

Critical: `userdata` must outlive the slot. Slots must be unref'd before
the userdata they reference is freed. Callbacks should check whether their
userdata is still valid.

## Async Method Calls

```c
r = sd_bus_call_method_async(bus, &slot,
                             dest, path, interface, member,
                             callback, userdata, types, ...);
```

Callbacks must handle all outcomes (success, error, timeout). The slot must
be tracked if cancellation might be needed.

## Error Replies

```c
/* Return error to caller */
return sd_bus_error_set_errno(error, r);

/* With custom message */
return sd_bus_error_set_errnof(error, r, "Failed to %s: %m", operation);

/* Well-known D-Bus error */
return sd_bus_error_set_const(error, SD_BUS_ERROR_INVALID_ARGS,
                              "Invalid argument");
```

All failure paths must set an `sd_bus_error`. Method callbacks must either
set an error or return a reply.

```c
static int method_callback(sd_bus_message *m, void *userdata,
                           sd_bus_error *error) {
        int r;

        r = do_something();
        if (r < 0)
                return sd_bus_error_set_errno(error, r);

        return sd_bus_reply_method_return(m, "");
}
```

## Object Vtables

```c
static const sd_bus_vtable manager_vtable[] = {
        SD_BUS_VTABLE_START(0),
        SD_BUS_METHOD("Reload", NULL, NULL, method_reload, 0),
        SD_BUS_PROPERTY("Version", "s", property_get_version, 0, 0),
        SD_BUS_VTABLE_END
};

r = sd_bus_add_object_vtable(bus, &slot, path,
                             interface, manager_vtable, userdata);
```

Vtables must be properly terminated. Slots must be tracked for cleanup.

## Object Lifecycle

Objects must be unregistered before their backing data is freed:

```c
r = sd_bus_add_object_vtable(bus, &slot, path, ...);
/* ... */
slot = sd_bus_slot_unref(slot);
/* Now safe to free userdata */
```

Asynchronous operations must be cancelled before cleanup.

## Property Get

```c
static int property_get_state(sd_bus *bus, const char *path,
                              const char *interface, const char *property,
                              sd_bus_message *reply, void *userdata,
                              sd_bus_error *error) {
        Unit *u = userdata;

        return sd_bus_message_append(reply, "s",
                                     unit_state_to_string(u->state));
}
```

The reply type must match the property signature.

## Property Change Notification

```c
r = sd_bus_emit_properties_changed(bus, path, interface,
                                   "PropertyName", NULL);
```

Only changed properties should be listed. Signals must not be emitted
before the object is registered.

## PID1-Specific

- **No blocking calls from PID1.** Never call `sd_bus_call()` from PID1;
  use async calls. Set timeouts appropriately.
- **Credential checking** for privileged operations:

  ```c
  r = sd_bus_query_sender_creds(m,
          SD_BUS_CREDS_PID|SD_BUS_CREDS_UID, &creds);
  if (r < 0)
          return r;

  r = sd_bus_creds_get_uid(creds, &uid);
  ```

  Consult PolicyKit when required.
