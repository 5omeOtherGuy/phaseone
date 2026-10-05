;; Regenerate: wasm-tools parse deadline-probe.wat -o deadline-probe.wasm
;; Pulse a synchronous host clock, then spin until fuel or deadline traps.
(component
  (import "p1:module/clock@1.0.0" (instance $clock
    (export "monotonic-now" (func (result u64)))))
  (alias export $clock "monotonic-now" (func $pulse))
  (core func $pulse-lowered (canon lower (func $pulse)))
  (core module $guest
    (import "host" "pulse" (func $pulse (result i64)))
    (memory (export "memory") 1)
    (func (export "realloc") (param i32 i32 i32 i32) (result i32)
      i32.const 0)
    (func (export "spin") (param i32 i32 i32 i32) (result i32)
      call $pulse
      drop
      (loop br 0)
      unreachable))
  (core instance $host (export "pulse" (func $pulse-lowered)))
  (core instance $instance (instantiate $guest (with "host" (instance $host))))
  (func $spin (param "first" string) (param "second" string) (result u32)
    (canon lift (core func $instance "spin")
      (memory (core memory $instance "memory")) (realloc (core func $instance "realloc"))))
  (export "spin" (func $spin)))
