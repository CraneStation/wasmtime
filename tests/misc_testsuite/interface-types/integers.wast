(@interface)

(module
 (@interface func (export "i32-to-s8") (param i32) (result s8)
   arg.get 0
   i32-to-s8)
 (@interface func (export "i32-to-s8x") (param i32) (result s8)
   arg.get 0
   i32-to-s8x)
 (@interface func (export "i32-to-u8") (param i32) (result u8)
   arg.get 0
   i32-to-u8)
 (@interface func (export "i32-to-s16") (param i32) (result s16)
   arg.get 0
   i32-to-s16)
 (@interface func (export "i32-to-s16x") (param i32) (result s16)
   arg.get 0
   i32-to-s16x)
 (@interface func (export "i32-to-u16") (param i32) (result u16)
   arg.get 0
   i32-to-u16)
 (@interface func (export "i32-to-s32") (param i32) (result s32)
   arg.get 0
   i32-to-s32)
 (@interface func (export "i32-to-u32") (param i32) (result u32)
   arg.get 0
   i32-to-u32)
 (@interface func (export "i32-to-s64") (param i32) (result s64)
   arg.get 0
   i32-to-s64)
 (@interface func (export "i32-to-u64") (param i32) (result u64)
   arg.get 0
   i32-to-u64)
)

(assert_return (invoke "i32-to-s8" (i32.const 0)) (s8.const 0))
(assert_return (invoke "i32-to-s8" (i32.const 8)) (s8.const 8))
(assert_return (invoke "i32-to-s8" (i32.const 0x100)) (s8.const 0))
(assert_return (invoke "i32-to-s8" (i32.const 0x10021)) (s8.const 0x21))
(assert_return (invoke "i32-to-s8" (i32.const -1)) (s8.const -1))
(assert_return (invoke "i32-to-s8" (i32.const 0xffffff00)) (s8.const 0))

(assert_return (invoke "i32-to-s8x" (i32.const 0)) (s8.const 0))
(assert_return (invoke "i32-to-s8x" (i32.const 8)) (s8.const 8))
(assert_return (invoke "i32-to-s8x" (i32.const -1)) (s8.const -1))
(assert_trap (invoke "i32-to-s8x" (i32.const 0x100)) "overflow")
(assert_trap (invoke "i32-to-s8x" (i32.const -129)) "overflow")

(assert_return (invoke "i32-to-u8" (i32.const 0)) (u8.const 0))
(assert_return (invoke "i32-to-u8" (i32.const 8)) (u8.const 8))
(assert_return (invoke "i32-to-u8" (i32.const 0x100)) (u8.const 0))
(assert_return (invoke "i32-to-u8" (i32.const 0x10021)) (u8.const 0x21))
(assert_return (invoke "i32-to-u8" (i32.const -1)) (u8.const 255))
(assert_return (invoke "i32-to-u8" (i32.const 0xffffff00)) (u8.const 0))

(assert_return (invoke "i32-to-s16" (i32.const 0)) (s16.const 0))
(assert_return (invoke "i32-to-s16" (i32.const 8)) (s16.const 8))
(assert_return (invoke "i32-to-s16" (i32.const 0x10000)) (s16.const 0))
(assert_return (invoke "i32-to-s16" (i32.const 0x1000021)) (s16.const 0x21))
(assert_return (invoke "i32-to-s16" (i32.const -1)) (s16.const -1))
(assert_return (invoke "i32-to-s16" (i32.const 0xffff0000)) (s16.const 0))

(assert_return (invoke "i32-to-s16x" (i32.const 0)) (s16.const 0))
(assert_return (invoke "i32-to-s16x" (i32.const 8)) (s16.const 8))
(assert_return (invoke "i32-to-s16x" (i32.const -1)) (s16.const -1))
(assert_trap (invoke "i32-to-s16x" (i32.const 0x10000)) "overflow")
(assert_trap (invoke "i32-to-s16x" (i32.const -32769)) "overflow")

(assert_return (invoke "i32-to-u16" (i32.const 0)) (u16.const 0))
(assert_return (invoke "i32-to-u16" (i32.const 8)) (u16.const 8))
(assert_return (invoke "i32-to-u16" (i32.const 0x10000)) (u16.const 0))
(assert_return (invoke "i32-to-u16" (i32.const 0x1000021)) (u16.const 0x21))
(assert_return (invoke "i32-to-u16" (i32.const -1)) (u16.const 65535))
(assert_return (invoke "i32-to-u16" (i32.const 0xffff0000)) (u16.const 0))

(assert_return (invoke "i32-to-s32" (i32.const 0)) (s32.const 0))
(assert_return (invoke "i32-to-s32" (i32.const 8)) (s32.const 8))
(assert_return (invoke "i32-to-s32" (i32.const 0x80000000)) (s32.const -0x80000000))
(assert_return (invoke "i32-to-s32" (i32.const 0x80000021)) (s32.const -0x7fffffdf))
(assert_return (invoke "i32-to-s32" (i32.const -1)) (s32.const -1))

(assert_return (invoke "i32-to-u32" (i32.const 0)) (u32.const 0))
(assert_return (invoke "i32-to-u32" (i32.const 8)) (u32.const 8))
(assert_return (invoke "i32-to-u32" (i32.const 0x80000000)) (u32.const 0x80000000))
(assert_return (invoke "i32-to-u32" (i32.const 0x80000021)) (u32.const 0x80000021))
(assert_return (invoke "i32-to-u32" (i32.const -1)) (u32.const 0xffffffff))

(assert_return (invoke "i32-to-s64" (i32.const 0)) (s64.const 0))
(assert_return (invoke "i32-to-s64" (i32.const 8)) (s64.const 8))
(assert_return (invoke "i32-to-s64" (i32.const 0x80000000)) (s64.const -0x80000000))
(assert_return (invoke "i32-to-s64" (i32.const 0x80000021)) (s64.const -0x7fffffdf))
(assert_return (invoke "i32-to-s64" (i32.const -1)) (s64.const -1))

(assert_return (invoke "i32-to-u64" (i32.const 0)) (u64.const 0))
(assert_return (invoke "i32-to-u64" (i32.const 8)) (u64.const 8))
(assert_return (invoke "i32-to-u64" (i32.const 0x80000000)) (u64.const 0xffffffff80000000))
(assert_return (invoke "i32-to-u64" (i32.const 0x80000021)) (u64.const 0xffffffff80000021))
(assert_return (invoke "i32-to-u64" (i32.const -1)) (u64.const 0xffffffffffffffff))