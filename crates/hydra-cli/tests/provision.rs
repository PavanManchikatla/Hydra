//! Provisioning (2026-09-02): the fence is minted once, the stage table and bootstraps are written
//! 0600, and `read_cluster` reads back exactly what `provision` wrote.

use std::os::unix::fs::PermissionsExt;

#[test]
fn provision_writes_the_fence_stages_and_bootstraps_and_reads_them_back() {
    let dir = tempfile::tempdir().unwrap();
    let ca = hydra_transport::ClusterCa::new().unwrap();
    ca.save_private(&dir.path().join("coordinator")).unwrap();
    let model = dir.path().join("m.gguf");
    std::fs::write(&model, b"only the hash is read here").unwrap();
    let stages = vec![
        hydra_cli::provision::StageSpec { name: "worker-s1".into(), rank: 0, addr: "127.0.0.1:9001".parse().unwrap() },
        hydra_cli::provision::StageSpec { name: "worker-s2".into(), rank: 1, addr: "127.0.0.1:9002".parse().unwrap() },
    ];
    let files = hydra_cli::provision::provision(dir.path(), model.to_str().unwrap(), &stages, Some(7), 256).expect("provision");
    let back = hydra_cli::provision::read_cluster(dir.path()).expect("read back");
    assert_eq!(back.fence, files.fence, "the fence round-trips");
    assert_eq!(back.model_path, model.to_str().unwrap());
    assert_eq!(back.n_ctx, 256);
    assert_eq!(back.stages.len(), 2);
    assert_eq!(back.stages[1].addr, "127.0.0.1:9002".parse().unwrap());
    for f in ["cluster.fence", "stages", "worker-s1.boot", "worker-s2.boot"] {
        let mode = std::fs::metadata(dir.path().join(f)).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "{f} must be 0600, got {mode:o}");
    }
    // The bootstraps carry the SAME fence and the split as given.
    let b1 = hydra_worker::bootstrap::Bootstrap::read_from(dir.path().join("worker-s1.boot").to_str().unwrap()).unwrap();
    let b2 = hydra_worker::bootstrap::Bootstrap::read_from(dir.path().join("worker-s2.boot").to_str().unwrap()).unwrap();
    assert_eq!(b1.cfg.fence, files.fence);
    assert_eq!(b2.cfg.fence, files.fence);
    assert_eq!((b1.cfg.layer_first, b1.cfg.layer_last, b1.cfg.is_final), (0, 7, false));
    assert_eq!((b2.cfg.layer_first, b2.cfg.layer_last, b2.cfg.is_final), (7, -1, true));
    assert!(b2.cfg.sampler_config.is_some() && b1.cfg.sampler_config.is_none(), "the sampler lives on S_P only");
    // A second provisioning mints a DIFFERENT session id (M12: CSPRNG, never reused by accident).
    let again = hydra_cli::provision::provision(dir.path(), model.to_str().unwrap(), &stages, Some(7), 256).expect("re-provision");
    assert_ne!(again.fence.session_id, files.fence.session_id);
}
