//! Not a test of the translator: what the host's compiler makes of DXIL.
//! `VICTORY_PROBE=<folder>` and `--ignored probe`: each `.dxil` there (a
//! compute program with a constant buffer `b0` of four dwords and a raw
//! buffer, slot 0 of the view heap) is run on the card. `sem*` programs are
//! given the bits of 0.0, infinity, -0.0 and a NaN and what they store is
//! printed; of the others the time is (several runs of 4096 groups).

use super::HostShader;
use super::host::{Dispatch, Host, Root, View};
use crate::native::{HwStage, RootType};

#[test]
#[ignore = "a tool: VICTORY_PROBE=<folder>"]
fn probe() {
    let Some(folder) = std::env::var_os("VICTORY_PROBE") else { return };
    let host = Host::new(false).expect("a card");
    let mut files: Vec<_> = std::fs::read_dir(folder)
        .expect("the folder")
        .filter_map(|e| Some(e.ok()?.path()))
        .filter(|p| p.extension().is_some_and(|e| e == "dxil"))
        .collect();
    files.sort();
    for file in files {
        let name = file.file_stem().unwrap_or_default().to_string_lossy().into_owned();
        let shader = HostShader {
            stage: HwStage::Cs,
            dxil: std::fs::read(&file).expect("the file"),
            user_data: (0..4)
                .map(|n| super::UserData { index: n, slot: 0, root: RootType::Constants, dword: n })
                .collect(),
            samplers: Vec::new(),
            static_samplers: Vec::new(),
            root_cbvs: Vec::new(),
            root_srvs: Vec::new(),
            root_uavs: Vec::new(),
            root_indexed_views: Vec::new(),
            half_views: Vec::new(),
            table_reads: None,
        };
        let (root, pso) = match host.compute_pipeline(&shader) {
            Ok(made) => made,
            Err(e) => {
                println!("{name}: {e}");
                continue;
            }
        };
        let run = |constants: [u32; 4], groups: u32| {
            let began = std::time::Instant::now();
            let out = host
                .dispatch(
                    &root,
                    &pso,
                    &Dispatch {
                        roots: vec![(0, Root::Constants(constants.to_vec()))],
                        views: &[View::RawUav(64)],
                        samplers: &[],
                        groups: [groups, 1, 1],
                    },
                )
                .expect("a dispatch");
            (out[0].clone(), began.elapsed().as_secs_f64() * 1e3)
        };
        if name.starts_with("sem") {
            let (out, _) = run([0, 0x7F80_0000, 0x8000_0000, 0x7FC0_0000], 1);
            for (n, x) in out.iter().enumerate().take(40) {
                println!("{name} {n:>2}: {x:08x} {}", f32::from_bits(*x));
            }
            continue;
        }
        let constants = [0.37f32.to_bits(), 0.11f32.to_bits(), 16384, 0];
        run(constants, 64);
        let mut times: Vec<f64> = (0..9).map(|_| run(constants, 4096).1).collect();
        times.sort_by(f64::total_cmp);
        println!("{name}: {:.2} ms (least {:.2})", times[times.len() / 2], times[0]);
    }
}

/// (Fox T) A tool: `VICTORY_INSPECT=<file.cdxil>[;...]` and `--ignored inspect`: what each
/// container's program is (wave64, threads, spilled, group barriers, ray-tracing node
/// tests, loops) and how it translates for a 32-lane host with `VICTORY_BVH_COUNT32` off
/// and on and `VICTORY_BVH_PAGE_REUSE` off and on: the wave size, whether it is counted in
/// threads, and the DXIL's size.
#[test]
#[ignore = "a tool: VICTORY_INSPECT=<file.cdxil>[;...]"]
fn inspect() {
    let Some(list) = std::env::var_os("VICTORY_INSPECT") else { return };
    super::lower::set_host_max_wave(32);
    for file in list.to_string_lossy().split(';').filter(|f| !f.is_empty()) {
        let bytes = std::fs::read(file).expect("the file");
        let c = crate::Container::parse(&bytes).expect("a container");
        let part = c.part(crate::FourCC::DXIL).or(c.part(crate::FourCC::ACCC)).expect("a program part");
        for native in crate::native_programs(part.data).expect("programs") {
            let Ok(p) = crate::spill::build(&native) else { continue };
            let names: Vec<&str> = p.blocks.iter().flat_map(|b| &b.insts).map(|i| i.name()).collect();
            let count = |n: &str| names.iter().filter(|x| x.contains(n)).count();
            let threads = match &native.io {
                crate::native::StageIo::Cs(cs) => Some(cs.threads),
                _ => None,
            };
            eprintln!(
                "{file}: tag {:x} stage {:?} wave64 {} threads {threads:?} spilled {} instructions {} s_barrier {} bvh_intersect {} global/flat loads {} buffer loads {} ds ops {} back-branches(blocks) {}",
                super::code_tag(&native.code),
                native.stage,
                native.wave64,
                crate::spill::is_spilled(&native),
                names.len(),
                count("s_barrier"),
                count("bvh"),
                names.iter().filter(|x| (x.starts_with("global_load") || x.starts_with("flat_load"))).count(),
                names.iter().filter(|x| x.starts_with("buffer_load") || x.starts_with("s_buffer_load")).count(),
                names.iter().filter(|x| x.starts_with("ds_")).count(),
                p.blocks.len(),
            );
            if std::env::var_os("VICTORY_INSPECT_DISASM").is_some() {
                for (n, i) in crate::gfx10::decode(&native.code).unwrap_or_default().iter().enumerate() {
                    eprintln!("    {n:4} {:#06x} {}", i.offset, i.text(native.wave64));
                }
            }
        }
        for (count32, reuse) in [(false, false), (true, false), (false, true)] {
            let was = (super::lower::TEST_COUNTED_HALVES.replace(Some(count32)), super::TEST_BVH_PAGE_REUSE.replace(Some(reuse)));
            match super::translate(&bytes) {
                Ok(s) => eprintln!(
                    "  COUNT32 {count32} PAGE_REUSE {reuse}: dxil {} bytes, validates {:?}, root uavs {:?}, counted in threads {}, user data {}",
                    s.dxil.len(),
                    super::validator::Validator::load(super::validator::Validator::default_path().as_deref())
                        .map(|mut v| v.validate(&mut s.dxil.clone()).map_err(|e| e.to_string()))
                        .ok(),
                    s.root_uavs,
                    s.user_data.iter().any(|u| u.root == RootType::Other(super::USER_DISPATCH_THREADS_X)),
                    s.user_data.len()
                ),
                Err(e) => eprintln!("  COUNT32 {count32} PAGE_REUSE {reuse}: does not translate: {e}"),
            }
            if let Ok(s) = super::translate(&bytes)
                && let Ok(host) = super::host::Host::new(false)
            {
                eprintln!("    the card makes its pipeline: {:?}", host.compute_pipeline(&s).map(|_| ()).map_err(|e| e.to_string()));
            }
            super::lower::TEST_COUNTED_HALVES.set(was.0);
            super::TEST_BVH_PAGE_REUSE.set(was.1);
        }
    }
}
