(component
  (import "p1:module/workspace-mutation@1.0.0" (instance $host
    (export "mutation" (type (sub resource)))
    (export "begin" (func (result (own 0))))))
  (alias export $host "mutation" (type $mutation))
  (alias export $host "begin" (func $begin))
  (core func $begin (canon lower (func $begin)))
  (core func $drop (canon resource.drop $mutation))
  (core module $probe
    (import "host" "begin" (func $begin (result i32)))
    (import "host" "drop" (func $drop (param i32)))
    (func (export "reenter") (result i32)
      (drop (call $begin))
      (call $begin))
    (func (export "release") (result i32)
      (call $drop (call $begin))
      (call $drop (call $begin))
      (i32.const 2)))
  (core instance $lowered
    (export "begin" (func $begin))
    (export "drop" (func $drop)))
  (core instance $probe (instantiate $probe (with "host" (instance $lowered))))
  (func (export "reenter") (result u32) (canon lift (core func $probe "reenter")))
  (func (export "release") (result u32) (canon lift (core func $probe "release"))))
