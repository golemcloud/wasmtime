use wasmtime::{Engine, Result, Store};
use wasmtime_wizer::Wizer;

#[tokio::test]
async fn preserve_null_continuation_roots() -> Result<()> {
    let mut config = wasmtime::Config::new();
    config.wasm_stack_switching(true).wasm_exceptions(true);
    let engine = Engine::new(&config)?;
    for heap in ["cont", "nocont", "$c"] {
        let body = format!(
            r#"
        (type $f (func))
        (type $c (cont $f))
        (global $immutable (ref null {heap}) (ref.null {heap}))
        (global $root (mut (ref null {heap})) (ref.null {heap}))
        (func (export "wizer-initialize"))
        (func (export "read") (result i32)
            (i32.and (ref.is_null (global.get $root)) (ref.is_null (global.get $immutable))))"#,
        );
        let source = wat::parse_str(format!("(module {body})"))?;
        let module = wasmtime::Module::new(&engine, &source)?;
        let mut store = Store::new(&engine, ());
        let original = wasmtime::Instance::new_async(&mut store, &module, &[]).await?;
        assert_eq!(
            original
                .get_typed_func::<(), i32>(&mut store, "read")?
                .call_async(&mut store, ())
                .await?,
            1
        );
        let output = Wizer::new()
            .run(&mut store, &source, async |store, module| {
                wasmtime::Instance::new_async(store, module, &[]).await
            })
            .await
            .unwrap_or_else(|error| panic!("null {heap} root must round-trip: {error:?}"));
        drop(store);
        let module = wasmtime::Module::new(&engine, output)?;
        let mut store = Store::new(&engine, ());
        let restored = wasmtime::Instance::new_async(&mut store, &module, &[]).await?;
        assert_eq!(
            restored
                .get_typed_func::<(), i32>(&mut store, "read")?
                .call_async(&mut store, ())
                .await?,
            1
        );

        #[cfg(feature = "component-model")]
        {
            use wasmtime::component::{Component, Linker};
            let source = wat::parse_str(format!(
                r#"(component
                (core module $m {body})
                (core instance $i (instantiate $m))
                (func (export "initialize") (canon lift (core func $i "wizer-initialize")))
                (func (export "read") (result u32) (canon lift (core func $i "read"))))"#
            ))?;
            let linker = Linker::<()>::new(&engine);
            let mut store = Store::new(&engine, ());
            let (output, _) = Wizer::new()
                .init_func("initialize")
                .run_component(&mut store, &source, async |store, component| {
                    linker.instantiate_async(store, component).await
                })
                .await?;
            drop(store);
            let mut store = Store::new(&engine, ());
            let component = Component::new(&engine, output)?;
            let restored = linker.instantiate_async(&mut store, &component).await?;
            assert_eq!(
                restored
                    .get_typed_func::<(), (u32,)>(&mut store, "read")?
                    .call_async(&mut store, ())
                    .await?
                    .0,
                1
            );
        }
    }
    Ok(())
}

#[tokio::test]
async fn preserve_state_without_unrelated_engine_features() -> Result<()> {
    for (gc, simd, body, expected) in [
        (
            false,
            false,
            r#"
            (global $root (mut i32) (i32.const 11))
            (func (export "wizer-initialize") (global.set $root (i32.const 43)))
            (func (export "read") (result i32) (global.get $root))"#,
            43,
        ),
        (
            true,
            false,
            r#"
            (type $node (struct (field (mut i32))))
            (global $root (ref $node) (struct.new $node (i32.const 17)))
            (func (export "wizer-initialize") (struct.set $node 0 (global.get $root) (i32.const 57)))
            (func (export "read") (result i32) (struct.get $node 0 (global.get $root)))"#,
            57,
        ),
        (
            true,
            true,
            r#"
            (type $node (struct (field (mut v128))))
            (global $root (ref $node) (struct.new $node (v128.const i32x4 3 5 7 11)))
            (func (export "wizer-initialize")
                (struct.set $node 0 (global.get $root) (v128.const i32x4 13 17 19 23)))
            (func (export "read") (result i32)
                (i32x4.extract_lane 2 (struct.get $node 0 (global.get $root))))"#,
            19,
        ),
        (
            true,
            true,
            r#"
            (type $vectors (array (mut v128)))
            (global $root (ref $vectors) (array.new $vectors (v128.const i32x4 3 5 7 11) (i32.const 2)))
            (func (export "wizer-initialize")
                (array.set $vectors (global.get $root) (i32.const 1) (v128.const i32x4 13 17 19 23)))
            (func (export "read") (result i32)
                (i32.add
                    (i32x4.extract_lane 0 (array.get $vectors (global.get $root) (i32.const 0)))
                    (i32x4.extract_lane 3 (array.get $vectors (global.get $root) (i32.const 1)))))"#,
            26,
        ),
        (
            true,
            false,
            r#"
            (type $node (struct (field (mut i32))))
            (global $root (ref $node) (struct.new $node (i32.const 17)))
            (global $exception exnref (ref.null exn))
            (global $mutable-exception (mut exnref) (ref.null exn))
            (func (export "wizer-initialize") (struct.set $node 0 (global.get $root) (i32.const 77)))
            (func (export "read") (result i32)
                (if (result i32) (i32.and (ref.is_null (global.get $exception)) (ref.is_null (global.get $mutable-exception)))
                    (then (struct.get $node 0 (global.get $root)))
                    (else (i32.const -1))))"#,
            77,
        ),
    ] {
        let mut config = wasmtime::Config::new();
        config
            .wasm_gc(gc)
            .wasm_simd(simd)
            .wasm_relaxed_simd(simd)
            .wasm_exceptions(true);
        let engine = Engine::new(&config)?;
        let source = wat::parse_str(format!("(module {body})"))?;
        let mut store = Store::new(&engine, ());
        let output = Wizer::new()
            .run(&mut store, &source, async |store, module| {
                wasmtime::Instance::new_async(store, module, &[]).await
            })
            .await?;
        drop(store);
        let mut store = Store::new(&engine, ());
        let module = wasmtime::Module::new(&engine, output)?;
        let instance = wasmtime::Instance::new_async(&mut store, &module, &[]).await?;
        assert_eq!(
            instance
                .get_typed_func::<(), i32>(&mut store, "read")?
                .call_async(&mut store, ())
                .await?,
            expected
        );

        #[cfg(feature = "component-model")]
        {
            use wasmtime::component::{Component, Linker};
            let source = wat::parse_str(format!(
                r#"(component
                (core module $m {body})
                (core instance $i (instantiate $m))
                (func (export "initialize") (canon lift (core func $i "wizer-initialize")))
                (func (export "read") (result u32) (canon lift (core func $i "read"))))"#
            ))?;
            let linker = Linker::<()>::new(&engine);
            let mut store = Store::new(&engine, ());
            let (output, _) = Wizer::new()
                .init_func("initialize")
                .run_component(&mut store, &source, async |store, component| {
                    linker.instantiate_async(store, component).await
                })
                .await?;
            drop(store);
            let mut store = Store::new(&engine, ());
            let component = Component::new(&engine, output)?;
            let instance = linker.instantiate_async(&mut store, &component).await?;
            assert_eq!(
                instance
                    .get_typed_func::<(), (u32,)>(&mut store, "read")?
                    .call_async(&mut store, ())
                    .await?
                    .0,
                expected as u32
            );
        }
    }
    Ok(())
}

#[tokio::test]
async fn reject_non_null_exception_roots() -> Result<()> {
    let mut config = wasmtime::Config::new();
    config.wasm_exceptions(true);
    let engine = Engine::new(&config)?;
    let source = wat::parse_str(
        r#"(module
        (tag $exception (param i32))
        (global $root (export "root") (mut exnref) (ref.null exn))
        (func (export "wizer-initialize")
            (global.set $root
                (block $caught (result exnref)
                    (try_table (catch_all_ref $caught) (throw $exception (i32.const 99)))
                    unreachable))))"#,
    )?;
    let mut store = Store::new(&engine, ());
    let module = wasmtime::Module::new(&engine, &source)?;
    let instance = wasmtime::Instance::new_async(&mut store, &module, &[]).await?;
    instance
        .get_typed_func::<(), ()>(&mut store, "wizer-initialize")?
        .call_async(&mut store, ())
        .await?;
    assert!(
        instance
            .get_global(&mut store, "root")
            .unwrap()
            .get(&mut store)
            .unwrap_exnref()
            .is_some()
    );
    let result = Wizer::new()
        .run(&mut store, &source, async |store, module| {
            wasmtime::Instance::new_async(store, module, &[]).await
        })
        .await;
    assert!(
        result
            .unwrap_err()
            .to_string()
            .contains("unsupported host-owned reference")
    );
    Ok(())
}

#[tokio::test]
async fn reject_unknown_host_gc_subtypes_without_truncating_known_subtypes() -> Result<()> {
    let engine = Engine::default();
    for known in [false, true] {
        let derived = "(type $derived (sub $base (struct (field i32) (field i32))))";
        let source = wat::parse_str(format!(
            r#"(module
            (type $base (sub (struct (field i32))))
            {}
            (import "host" "make" (func $make (result (ref $base))))
            (global $root (export "root") (mut (ref null $base)) (ref.null $base))
            (func (export "wizer-initialize") (global.set $root (call $make))))"#,
            if known { derived } else { "" }
        ))?;
        let producer = wasmtime::Module::new(
            &engine,
            format!(
                r#"(module
            (type $base (sub (struct (field i32)))) {derived}
            (global (export "root") (ref $derived)
                (struct.new $derived (i32.const 17) (i32.const 99))))"#
            ),
        )?;
        let mut store = Store::new(&engine, ());
        let producer = wasmtime::Instance::new(&mut store, &producer, &[])?;
        let root = producer
            .get_global(&mut store, "root")
            .unwrap()
            .get(&mut store);
        let module = wasmtime::Module::new(&engine, &source)?;
        let wasmtime::ExternType::Func(ty) = module.imports().next().unwrap().ty() else {
            unreachable!()
        };
        let mut linker = wasmtime::Linker::new(&engine);
        linker.func_new("host", "make", ty, move |_, _, results| {
            results[0] = root;
            Ok(())
        })?;
        let output = Wizer::new()
            .run(&mut store, &source, async |store, module| {
                linker.instantiate_async(store, module).await
            })
            .await;
        if !known {
            let error = output.expect_err("an unknown subtype must not be silently truncated");
            assert!(error.to_string().contains("unknown host-owned GC subtype"));
            continue;
        }
        let output = wasmtime::Module::new(&engine, output?)?;
        let restored = linker.instantiate(&mut store, &output)?;
        let root = restored
            .get_global(&mut store, "root")
            .unwrap()
            .get(&mut store)
            .unwrap_any_ref()
            .unwrap()
            .unwrap_struct(&store)?;
        assert_eq!(root.ty(&store)?.fields().len(), 2);
        assert_eq!(root.field(&mut store, 0)?.unwrap_i32(), 17);
        assert_eq!(root.field(&mut store, 1)?.unwrap_i32(), 99);
    }
    Ok(())
}

#[tokio::test]
async fn preserve_host_gc_signatures_and_function_identity() -> Result<()> {
    let original = wat::parse_str(
        r#"(module
      (rec (type $never (struct (field (ref $never)))))
      (type $f (func (param (ref $never)) (result i32)))
      (rec (type $node (struct (field (mut (ref null $node))) (field (mut i32)) (field (mut (ref $f))))))
      (import "host" "roundtrip" (func $roundtrip (param (ref $node) (ref $f)) (result (ref $node))))
      (func $f (export "f") (type $f) (param (ref $never)) (result i32) unreachable)
      (func $g (export "g") (type $f) (param (ref $never)) (result i32) unreachable)
      (global $root (export "root") (ref $node) (struct.new $node (ref.null $node) (i32.const 91) (ref.func $f)))
      (global $alias (mut (ref null $node)) (ref.null $node))
      (func (export "wizer-initialize") (global.set $alias (call $roundtrip (global.get $root) (ref.func $g))))
      (func (export "read") (result i32) (struct.get $node 1 (global.get $root)))
      (func (export "identity") (result i32)
        (i32.and (ref.eq (global.get $root) (global.get $alias))
          (ref.eq (struct.get $node 0 (global.get $root)) (global.get $root)))))"#,
    )?;
    let engine = Engine::default();
    let module = wasmtime::Module::new(&engine, &original)?;
    let wasmtime::ExternType::Func(ty) = module.imports().next().unwrap().ty() else {
        unreachable!()
    };
    let mut linker = wasmtime::Linker::<u32>::new(&engine);
    linker.func_new("host", "roundtrip", ty, |mut caller, args, results| {
        let node = args[0].unwrap_any_ref().unwrap().unwrap_struct(&caller)?;
        assert_eq!(node.field(&mut caller, 1)?.unwrap_i32(), 91);
        node.set_field(&mut caller, 0, args[0])?;
        node.set_field(&mut caller, 1, wasmtime::Val::I32(71))?;
        node.set_field(&mut caller, 2, args[1])?;
        results[0] = args[0];
        *caller.data_mut() += 1;
        Ok(())
    })?;
    let mut store = Store::new(&engine, 0);
    let restored = Wizer::new()
        .run(&mut store, &original, async |store, module| {
            linker.instantiate_async(store, module).await
        })
        .await?;
    assert_eq!(*store.data(), 1);
    drop(store);
    let module = wasmtime::Module::new(&engine, restored)?;
    let mut store = Store::new(&engine, 0);
    let instance = linker.instantiate_async(&mut store, &module).await?;
    store.gc(None)?;
    assert_eq!(*store.data(), 0);
    assert_eq!(
        instance
            .get_typed_func::<(), i32>(&mut store, "read")?
            .call_async(&mut store, ())
            .await?,
        71
    );
    assert_eq!(
        instance
            .get_typed_func::<(), i32>(&mut store, "identity")?
            .call_async(&mut store, ())
            .await?,
        1
    );
    let root = instance
        .get_global(&mut store, "root")
        .unwrap()
        .get(&mut store)
        .unwrap_any_ref()
        .unwrap()
        .unwrap_struct(&store)?;
    let retained = root
        .field(&mut store, 2)?
        .unwrap_funcref()
        .unwrap()
        .to_raw(&mut store);
    assert_eq!(
        retained,
        instance
            .get_func(&mut store, "g")
            .unwrap()
            .to_raw(&mut store)
    );
    assert_ne!(
        retained,
        instance
            .get_func(&mut store, "f")
            .unwrap()
            .to_raw(&mut store)
    );
    Ok(())
}

#[cfg(feature = "component-model")]
#[tokio::test]
async fn preserve_component_gc_graph_across_modules() -> Result<()> {
    use wasmtime::component::{Component, Linker};
    let types = r#"
      (type $f (func (result i32)))
      (type $bytes (array (mut i8)))
      (type $node (struct (field (mut (ref null $node))) (field (ref $bytes)) (field (ref $f))))"#;
    let source = wat::parse_str(format!(
        r#"(component
      (import "tick" (func $tick))
      (core func $tick (canon lower (func $tick)))
      (core instance $host (export "tick" (func $tick)))
      (core module $a-module {types}
        (table (export "sources") 1 (ref null $node))
        (global $bytes (ref $bytes) (array.new $bytes (i32.const 93) (i32.const 3)))
        (func $f (type $f) (result i32) (i32.const 17))
        (elem declare func $f)
        (func (export "new") (result (ref $node))
          (struct.new $node (ref.null $node) (global.get $bytes) (ref.func $f))))
      (core instance $unused)
      (core instance $a (instantiate $a-module
        (with "__wizer_gc_state" (instance $unused))
        (with "wizer-gc-state-0" (instance $unused))))
      (core module $b-module {types}
        (type $roots (array (mut (ref $bytes))))
        (type $callbacks (array (mut (ref $f))))
        (import "host" "tick" (func $tick))
        (import "a" "new" (func $new (result (ref $node))))
        (import "a" "sources" (table $sources 1 (ref null $node)))
        (table $local 1 (ref null $node))
        (global $anchor (ref $node)
          (struct.new $node (ref.null $node) (array.new $bytes (i32.const 11) (i32.const 3)) (ref.func $f)))
        (global $root (mut (ref null $node)) (ref.null $node))
        (elem $saved (ref $bytes) (array.new_fixed $bytes 2 (i32.const 41) (i32.const 82)))
        (elem $functions (ref $f) (ref.func $f))
        (func $f (type $f) (result i32) (i32.const 29))
        (func (export "init")
          (call $tick)
          (array.set $bytes
            (array.get $roots (array.new_elem $roots $saved (i32.const 0) (i32.const 1)) (i32.const 0))
            (i32.const 1) (i32.const 203))
          (global.set $root (call $new))
          (struct.set $node 0 (global.get $root) (global.get $anchor))
          (struct.set $node 0 (global.get $anchor) (global.get $root))
          (array.set $bytes (struct.get $node 1 (global.get $root)) (i32.const 1) (i32.const 42))
          (drop (table.grow $sources (ref.null $node) (i32.const 2)))
          (table.set $sources (i32.const 2) (global.get $root))
          (table.set $local (i32.const 0) (global.get $anchor)))
        (func (export "read") (result i32)
          (if (result i32)
            (i32.and
              (i32.and (ref.eq (struct.get $node 0 (global.get $root)) (global.get $anchor))
                (ref.eq (struct.get $node 0 (global.get $anchor)) (global.get $root)))
              (i32.and (ref.eq (table.get $sources (i32.const 2)) (global.get $root))
                (ref.eq (table.get $local (i32.const 0)) (global.get $anchor))))
            (then (i32.add
              (i32.add
                (array.get_u $bytes (array.get $roots (array.new_elem $roots $saved (i32.const 0) (i32.const 1)) (i32.const 0)) (i32.const 1))
                (call_ref $f (array.get $callbacks (array.new_elem $callbacks $functions (i32.const 0) (i32.const 1)) (i32.const 0))))
              (i32.add
                (i32.add
                  (array.get_u $bytes (struct.get $node 1 (global.get $root)) (i32.const 0))
                  (array.get_u $bytes (struct.get $node 1 (global.get $root)) (i32.const 1)))
                (i32.add
                  (array.get_u $bytes (struct.get $node 1 (global.get $anchor)) (i32.const 0))
                  (i32.add (call_ref $f (struct.get $node 2 (global.get $root)))
                    (call_ref $f (struct.get $node 2 (global.get $anchor))))))))
            (else (i32.const -1))))
        (func (export "mutate")
          (array.set $bytes (struct.get $node 1 (global.get $root)) (i32.const 0) (i32.const 201))))
      (core instance $b (instantiate $b-module (with "host" (instance $host)) (with "a" (instance $a))))
      (alias core export $b "init" (core func $init))
      (alias core export $b "read" (core func $read))
      (alias core export $b "mutate" (core func $mutate))
      (func (export "wizer-initialize") (canon lift (core func $init)))
      (func (export "read") (result u32) (canon lift (core func $read)))
      (func (export "mutate") (canon lift (core func $mutate))))"#
    ))?;
    let engine = Engine::default();
    let mut linker = Linker::<u32>::new(&engine);
    linker.root().func_wrap("tick", |mut store, (): ()| {
        *store.data_mut() += 1;
        Ok(())
    })?;
    let mut store = Store::new(&engine, 0);
    let (output, _) = Wizer::new()
        .run_component(&mut store, &source, async |store, component| {
            linker.instantiate_async(store, component).await
        })
        .await?;
    assert_eq!(*store.data(), 1);
    drop(store);
    let mut store = Store::new(&engine, 0);
    let component = Component::new(&engine, &output)?;
    let instance = linker.instantiate_async(&mut store, &component).await?;
    store.gc(None)?;
    assert_eq!(*store.data(), 0);
    let read = instance.get_typed_func::<(), (u32,)>(&mut store, "read")?;
    assert_eq!(read.call_async(&mut store, ()).await?.0, 424);
    instance
        .get_typed_func::<(), ()>(&mut store, "mutate")?
        .call_async(&mut store, ())
        .await?;
    store.gc(None)?;
    assert_eq!(read.call_async(&mut store, ()).await?.0, 532);
    assert_eq!(*store.data(), 0);
    drop(store);

    let mut output = output;
    for _ in 0..2 {
        let mut store = Store::new(&engine, 0);
        (output, _) = Wizer::new()
            .init_func("mutate")
            .keep_init_func(true)
            .run_component(&mut store, &output, async |store, component| {
                linker.instantiate_async(store, component).await
            })
            .await?;
        assert_eq!(*store.data(), 0);
        drop(store);
        let mut store = Store::new(&engine, 0);
        let component = Component::new(&engine, &output)?;
        let instance = linker.instantiate_async(&mut store, &component).await?;
        store.gc(None)?;
        let read = instance.get_typed_func::<(), (u32,)>(&mut store, "read")?;
        assert_eq!(read.call_async(&mut store, ()).await?.0, 532);
        assert_eq!(*store.data(), 0);
    }
    Ok(())
}

#[cfg(feature = "component-model")]
#[tokio::test]
async fn preserve_gc_initialized_by_indirect_thread() -> Result<()> {
    use wasmtime::component::{Component, Linker};
    let mut config = wasmtime::Config::new();
    config.wasm_component_model_threading(true);
    let engine = Engine::new(&config)?;
    let source = wat::parse_str(
        r#"(component
      (core module $tables
        (table (export "first") 2 funcref)
        (table (export "second") 2 funcref))
      (core instance $tables (instantiate $tables))
      (core type $entry (func (param i32)))
      (core func $first (canon thread.new-indirect $entry (core table $tables "first")))
      (core func $second (canon thread.new-indirect $entry (core table $tables "second")))
      (core func $resume (canon thread.yield-then-resume))
      (core module $m
        (type $node (struct (field (mut i32))))
        (import "host" "first" (func $first (param i32 i32) (result i32)))
        (import "host" "second" (func $second (param i32 i32) (result i32)))
        (import "host" "resume" (func $resume (param i32) (result i32)))
        (import "tables" "first" (table $first-table 2 funcref))
        (import "tables" "second" (table $second-table 2 funcref))
        (global $root (ref $node) (struct.new $node (i32.const 0)))
        (func $a (param i32) (struct.set $node 0 (global.get $root) (i32.add (local.get 0) (i32.const 7))))
        (func $b (param i32) (struct.set $node 0 (global.get $root) (i32.add (local.get 0) (i32.const 29))))
        (elem (table $first-table) (i32.const 1) func $a)
        (elem (table $second-table) (i32.const 1) func $a)
        (elem declare func $b)
        (func (export "init")
          (drop (call $resume (call $first (i32.const 1) (i32.const 10))))
          (table.set $second-table (i32.const 1) (ref.func $b))
          (drop (call $resume (call $second (i32.const 1) (i32.const 62)))))
        (func (export "read") (result i32) (struct.get $node 0 (global.get $root))))
      (core instance $i (instantiate $m
        (with "host" (instance (export "first" (func $first)) (export "second" (func $second)) (export "resume" (func $resume))))
        (with "tables" (instance $tables))))
      (func (export "wizer-initialize") async (canon lift (core func $i "init")))
      (func (export "read") (result u32) (canon lift (core func $i "read"))))"#,
    )?;
    let linker = Linker::<()>::new(&engine);
    let mut store = Store::new(&engine, ());
    let (output, _) = Wizer::new()
        .run_component(&mut store, &source, async |store, component| {
            linker.instantiate_async(store, component).await
        })
        .await?;
    drop(store);
    let mut store = Store::new(&engine, ());
    let component = Component::new(&engine, output)?;
    let instance = linker.instantiate_async(&mut store, &component).await?;
    store.gc(None)?;
    assert_eq!(
        instance
            .get_typed_func::<(), (u32,)>(&mut store, "read")?
            .call_async(&mut store, ())
            .await?
            .0,
        91
    );
    Ok(())
}
