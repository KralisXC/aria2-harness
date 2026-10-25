;; com.test.wasm-ctx —— Wasm 强隔离后端「完整 ctx 通路」探针（增量 34 / 35）
;;
;; 目的：证明 Wasm 插件与 JS 插件享有**同等的 ctx 能力**。全部能力统一走
;; `env.host_call` 的 JSON 编解码机制（op 名与 JS 侧 `ctx.*` 同源），宿主侧不引入
;; 任何新的 ABI 映射 —— 类型化导出留在 Wasm 内部由插件自己解析 JSON。
;;
;; 每条通路一个独立 hook，便于宿主侧做**内容级**断言：
;;
;;   onAppStart          真实场景：log -> notify -> getTask -> settings.get（串行）
;;   onDownloadComplete  真实下载完成：settings.get -> log(原样 JSON) -> notify -> getTask(真实 gid) -> log
;;   onDownloadError     失败路径：log -> getTask(真实 gid) -> log
;;   probeNotify         仅调用 ctx.notify
;;   probeGetTask        仅调用 ctx.getTask（回包指针写入 last_result_ptr）
;;   probeSettings       仅调用 ctx.settings.get（回包指针写入 last_settings_ptr）
;;   probeSpin           故意死循环，验证宿主 fuel 截断（刻意不写进 manifest.hooks）
;;
;; 与宿主约定的 ABI（见 src-host/src/plugin/wasm_runtime.rs）：
;;   export "memory"             : 宿主读写载荷/结果用的线性内存
;;   export "harness_alloc"      : (i32 size) -> i32 ptr   （宿主写 JSON / 回包前申请）
;;   export "harness_free"       : (i32 ptr)               （bump 分配器，no-op）
;;   export "harness_call"       : (hook_ptr,hook_len,payload_ptr,payload_len) -> i32
;;   export "last_result_ptr"    : (mut i32) 最近一次 getTask 回包指针（0 = 无回包）
;;   export "last_settings_ptr"  : (mut i32) 最近一次 settings.get 回包指针
;;                                 —— 探针专用观测出口：宿主读它再读线性内存，
;;                                 即可对回包内容做逐字节断言。
;;   import "env"."log"          : (level_ptr,level_len,msg_ptr,msg_len)
;;   import "env"."host_call"    : (op_ptr,op_len,payload_ptr,payload_len) -> i32 result_ptr
;;
;; 事件 hook 的 payload 由宿主 dispatcher 固定产出为 `{"hook":"<name>","gid":"<16 位 hex>"}`
;; （见 plugin/dispatcher.rs:82）。注意 serde_json 默认 Map 是 BTreeMap，**键按字典序
;; 序列化**，实际字节序是 `{"gid":"<gid>","hook":"<name>"}` —— 即 gid 固定落在偏移 8。
;; 因此「找 gid」无需 JSON 解析器（见 $copy_gid 调用点的 `$pp + 8`）。
;;
;; 注意：本模块**不含**越界指针的攻击路径。越界 / 超长 / 负指针 / 越界回写 /
;; memory.grow 爆炸这五类防御，由宿主单测里的恶意模块覆盖（见
;; wasm_runtime.rs::tests），避免正常探针在运行期把熔断计数刷满。
(module
  ;; ---- 宿主注入的能力 ----
  (import "env" "log" (func $host_log (param i32 i32 i32 i32)))
  (import "env" "host_call" (func $host_call (param i32 i32 i32 i32) (result i32)))

  (memory (export "memory") 1)

  ;; ---- 静态字符串区（4096 起；bump 分配器从 8192 起，互不重叠）----
  ;; 载荷
  (data (i32.const 4096) "{\"key\":\"greeting\"}")                        ;; 18
  (data (i32.const 4128) "{\"title\":\"Wasm\",\"body\":\"ctx online\"}")   ;; 36
  (data (i32.const 4168) "{\"gid\":\"probe-gid\"}")                        ;; 19
  ;; op 名（与 JS 侧 ctx.* 同源）
  (data (i32.const 4192) "log")                                            ;;  3
  (data (i32.const 4200) "notify")                                         ;;  6
  (data (i32.const 4216) "getTask")                                        ;;  7
  (data (i32.const 4232) "settings.get")                                   ;; 12
  ;; hook 名
  (data (i32.const 4256) "onAppStart")                                     ;; 10
  (data (i32.const 4272) "onShutdown")                                     ;; 10
  (data (i32.const 4288) "probeNotify")                                    ;; 11
  (data (i32.const 4304) "probeGetTask")                                   ;; 12
  (data (i32.const 4320) "probeSettings")                                  ;; 13
  (data (i32.const 4344) "probeSpin")                                      ;;  9
  ;; 日志
  (data (i32.const 4360) "wasm-ctx probe: onAppStart")                     ;; 26
  (data (i32.const 4400) "info")                                           ;;  4
  ;; 增量 35：下载事件 hook（供全链路回归验证真实 gid / settings）
  (data (i32.const 4416) "onDownloadComplete")                             ;; 18
  (data (i32.const 4440) "onDownloadError")                                ;; 15
  (data (i32.const 4464) "wasm-ctx probe: onDownloadComplete")             ;; 34
  (data (i32.const 4504) "wasm-ctx probe: onDownloadError")                ;; 31
  ;; getTask 载荷模板：16 个 '0' 会被 $copy_gid 覆写成真实 gid
  (data (i32.const 4608) "{\"gid\":\"0000000000000000\"}")                 ;; 27

  ;; ---- bump 分配器（只给宿主写 hook 名 / 载荷 / 回包用）----
  ;; 起点 8192 高于全部静态串（最高 4635），永不覆写。
  (global $next (mut i32) (i32.const 8192))

  (func (export "harness_alloc") (param $size i32) (result i32)
    (local $p i32)
    (local.set $p (global.get $next))
    (global.set $next
      (i32.add (global.get $next)
               (i32.and (i32.add (local.get $size) (i32.const 7)) (i32.const -8))))
    (local.get $p))

  (func (export "harness_free") (param i32))

  ;; ---- 探针观测出口：回包指针（0 = 宿主无返回值）----
  (global $last_result_ptr (export "last_result_ptr") (mut i32) (i32.const 0))
  (global $last_settings_ptr (export "last_settings_ptr") (mut i32) (i32.const 0))

  ;; ---- 逐字节比较：全等返回 1 ----
  (func $bytes_eq (param $a i32) (param $b i32) (param $n i32) (result i32)
    (local $i i32)
    (block $done
      (loop $l
        (br_if $done (i32.ge_u (local.get $i) (local.get $n)))
        (br_if $done
          (i32.ne (i32.load8_u (i32.add (local.get $a) (local.get $i)))
                  (i32.load8_u (i32.add (local.get $b) (local.get $i)))))
        (local.set $i (i32.add (local.get $i) (i32.const 1)))
        (br $l)))
    (i32.ge_u (local.get $i) (local.get $n)))

  ;; ---- hook 名比较（长度 + 内容双重校验）----
  (func $name_eq (param $ptr i32) (param $len i32) (param $off i32) (param $want i32) (result i32)
    (i32.and (i32.eq (local.get $len) (local.get $want))
             (call $bytes_eq (local.get $ptr) (local.get $off) (local.get $want))))

  ;; ---- NUL 结尾字符串长度（带上界，防宿主异常指针把探针拖死）----
  (func $strlen (param $p i32) (result i32)
    (local $i i32)
    (block $done
      (loop $l
        (br_if $done (i32.ge_u (local.get $i) (i32.const 4096)))
        (br_if $done (i32.eqz (i32.load8_u (i32.add (local.get $p) (local.get $i)))))
        (local.set $i (i32.add (local.get $i) (i32.const 1)))
        (br $l)))
    (local.get $i))

  ;; ---- 把事件 payload 里的 16 位 gid 抄进模板，返回 getTask 载荷指针 ----
  ;; 模板固定 26 字节：`{"gid":"` + gid + `"}`（前缀 8 字节 → gid 落在 4608+8=4616）
  (func $copy_gid (param $src i32) (result i32)
    (local $i i32)
    (block $done
      (loop $l
        (br_if $done (i32.ge_u (local.get $i) (i32.const 16)))
        (i32.store8 (i32.add (i32.const 4616) (local.get $i))
                    (i32.load8_u (i32.add (local.get $src) (local.get $i))))
        (local.set $i (i32.add (local.get $i) (i32.const 1)))
        (br $l)))
    (i32.const 4608))

  ;; ---- 统一入口 ----
  (func (export "harness_call")
        (param $hp i32) (param $hl i32) (param $pp i32) (param $pl i32) (result i32)

    ;; (1) 真实场景：log -> notify -> getTask -> settings.get（串行）
    (if (call $name_eq (local.get $hp) (local.get $hl) (i32.const 4256) (i32.const 10))
      (then
        (call $host_log (i32.const 4400) (i32.const 4)
                        (i32.const 4360) (i32.const 26))
        (drop (call $host_call (i32.const 4200) (i32.const 6)
                               (i32.const 4128) (i32.const 36)))
        (global.set $last_result_ptr
          (call $host_call (i32.const 4216) (i32.const 7)
                           (i32.const 4168) (i32.const 19)))
        (global.set $last_settings_ptr
          (call $host_call (i32.const 4232) (i32.const 12)
                           (i32.const 4096) (i32.const 18)))))

    ;; (2) 下载完成：settings.get（原样回读）-> notify -> getTask（真实 gid）
    (if (call $name_eq (local.get $hp) (local.get $hl) (i32.const 4416) (i32.const 18))
      (then
        (call $host_log (i32.const 4400) (i32.const 4)
                        (i32.const 4464) (i32.const 34))
        (global.set $last_settings_ptr
          (call $host_call (i32.const 4232) (i32.const 12)
                           (i32.const 4096) (i32.const 18)))
        (if (global.get $last_settings_ptr)
          (then
            (call $host_log (i32.const 4400) (i32.const 4)
                            (global.get $last_settings_ptr)
                            (call $strlen (global.get $last_settings_ptr)))))
        (drop (call $host_call (i32.const 4200) (i32.const 6)
                               (i32.const 4128) (i32.const 36)))
        (global.set $last_result_ptr
          (call $host_call (i32.const 4216) (i32.const 7)
                           (call $copy_gid (i32.add (local.get $pp) (i32.const 8)))
                           (i32.const 26)))
        (if (global.get $last_result_ptr)
          (then
            (call $host_log (i32.const 4400) (i32.const 4)
                            (global.get $last_result_ptr)
                            (call $strlen (global.get $last_result_ptr)))))))

    ;; (3) 下载失败：log -> getTask（真实 gid）
    (if (call $name_eq (local.get $hp) (local.get $hl) (i32.const 4440) (i32.const 15))
      (then
        (call $host_log (i32.const 4400) (i32.const 4)
                        (i32.const 4504) (i32.const 31))
        (global.set $last_result_ptr
          (call $host_call (i32.const 4216) (i32.const 7)
                           (call $copy_gid (i32.add (local.get $pp) (i32.const 8)))
                           (i32.const 26)))
        (if (global.get $last_result_ptr)
          (then
            (call $host_log (i32.const 4400) (i32.const 4)
                            (global.get $last_result_ptr)
                            (call $strlen (global.get $last_result_ptr)))))))

    ;; (4) probeNotify：只调 notify（回包为空 → 忽略返回值）
    (if (call $name_eq (local.get $hp) (local.get $hl) (i32.const 4288) (i32.const 11))
      (then
        (drop (call $host_call (i32.const 4200) (i32.const 6)
                               (i32.const 4128) (i32.const 36)))))

    ;; (5) probeGetTask：回包指针落进 last_result_ptr
    (if (call $name_eq (local.get $hp) (local.get $hl) (i32.const 4304) (i32.const 12))
      (then
        (global.set $last_result_ptr
          (call $host_call (i32.const 4216) (i32.const 7)
                           (i32.const 4168) (i32.const 19)))))

    ;; (6) probeSettings：回包指针落进 last_settings_ptr
    (if (call $name_eq (local.get $hp) (local.get $hl) (i32.const 4320) (i32.const 13))
      (then
        (global.set $last_settings_ptr
          (call $host_call (i32.const 4232) (i32.const 12)
                           (i32.const 4096) (i32.const 18)))))

    ;; (7) probeSpin：死循环，供宿主 fuel 截断回归（不进 manifest.hooks）
    (if (call $name_eq (local.get $hp) (local.get $hl) (i32.const 4344) (i32.const 9))
      (then (loop $forever (br $forever))))

    (i32.const 0))
)
