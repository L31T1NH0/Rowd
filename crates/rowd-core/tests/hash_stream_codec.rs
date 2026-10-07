use rowd_core::{
    protocol::{self, Message},
    storage::{ScanStreamMetrics, StoreMetrics},
};

#[test]
fn hash_stream_end_round_trips_zero_common_and_large_metrics() -> anyhow::Result<()> {
    for value in [0_u64, 1_234, u64::MAX] {
        let original = Message::HashStreamEnd {
            scan_id: "scan".into(),
            last_sequence: 33,
            total_entries: 1040,
            metrics: StoreMetrics {
                files_enumerated: 1040,
                files_hashed: 6,
                bytes_hashed: 8192,
                staging_copies: 4,
                full_scans: 1,
            },
            pipeline: ScanStreamMetrics {
                namespace_ms: value.into(),
                time_to_first_hash_ms: Some(value.into()),
                queue_wait_ms: value.into(),
                hashes_calculated: 6,
                hashes_reused: 1034,
                files_staged_during_hash: 4,
                duplicate_reads_avoided: 4,
                queue_peak_chunks: 2,
                saf_source_lookup_fallback_count: 1,
            },
        };
        let json = serde_json::to_string(&original)?;
        let decoded: Message = serde_json::from_str(&json)?;
        let Message::HashStreamEnd {
            scan_id,
            last_sequence,
            total_entries,
            metrics,
            pipeline,
        } = decoded
        else {
            panic!("wrong message variant")
        };
        assert_eq!(scan_id, "scan");
        assert_eq!(last_sequence, 33);
        assert_eq!(total_entries, 1040);
        assert_eq!(u128::from(pipeline.namespace_ms), u128::from(value));
        assert_eq!(
            pipeline.time_to_first_hash_ms.map(u128::from),
            Some(u128::from(value))
        );
        assert_eq!(u128::from(pipeline.queue_wait_ms), u128::from(value));
        let decoded = Message::HashStreamEnd {
            scan_id,
            last_sequence,
            total_entries,
            metrics,
            pipeline,
        };
        assert_eq!(
            serde_json::to_value(&decoded)?,
            serde_json::to_value(&original)?
        );

        // Production frames nest the internally tagged message in Scoped.
        let mut frame = Vec::new();
        protocol::send_for(&mut frame, "share", original)?;
        let peer = protocol::receive_for(&mut std::io::Cursor::new(frame), "share")?;
        assert_eq!(
            serde_json::to_value(&peer)?,
            serde_json::to_value(&decoded)?
        );
    }
    let original = Message::HashStreamEnd {
        scan_id: "empty".into(),
        last_sequence: 0,
        total_entries: 0,
        metrics: Default::default(),
        pipeline: Default::default(),
    };
    let decoded: Message = serde_json::from_str(&serde_json::to_string(&original)?)?;
    assert_eq!(
        serde_json::to_value(&decoded)?,
        serde_json::to_value(&original)?
    );
    Ok(())
}

#[test]
fn wire_metrics_reject_overflow_instead_of_truncating() {
    for field in ["namespace_ms", "time_to_first_hash_ms", "queue_wait_ms"] {
        let json = format!(r#"{{"{field}":18446744073709551616}}"#);
        assert!(serde_json::from_str::<ScanStreamMetrics>(&json).is_err());
    }
}
