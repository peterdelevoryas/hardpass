use anyhow::Result;
use hardpass::{Hardpass, InstanceStatus, VmSpec};
use tempfile::tempdir;

#[tokio::test]
#[ignore = "requires QEMU and HARDPASS_REAL_QEMU_TEST=1"]
async fn library_api_smoke() -> Result<()> {
    if std::env::var_os("HARDPASS_REAL_QEMU_TEST").is_none() {
        eprintln!("skipping library_api_smoke; set HARDPASS_REAL_QEMU_TEST=1 to enable");
        return Ok(());
    }

    let root = tempdir()?;
    let hardpass = Hardpass::with_root(root.path()).await?;
    hardpass.doctor().await?;

    let name = format!("api_smoke_{}", std::process::id());
    let vm_handle = hardpass.vm(&name)?;
    let result = async {
        let vm = hardpass
            .create(
                VmSpec::new(&name)
                    .cpus(1)
                    .memory_mib(1024)
                    .disk_gib(8)
                    .timeout_secs(180),
            )
            .await?;

        let running = vm.start().await?;
        let info = running.wait_for_ssh().await?;
        assert_eq!(info.status, InstanceStatus::Running);

        let output = running.exec_checked(["uname", "-m"]).await?;
        assert_eq!(output.stdout.trim(), expected_guest_machine());

        let vm = running.stop().await?;
        assert_eq!(vm.status().await?, InstanceStatus::Stopped);
        Ok::<(), anyhow::Error>(())
    }
    .await;

    let cleanup = vm_handle.delete().await;
    match (result, cleanup) {
        (Err(err), _) => Err(err),
        (Ok(()), Err(err)) => Err(err),
        (Ok(()), Ok(())) => Ok(()),
    }
}

fn expected_guest_machine() -> &'static str {
    match std::env::consts::ARCH {
        "x86_64" => "x86_64",
        "aarch64" | "arm64" => "aarch64",
        other => panic!("unsupported host architecture for smoke test: {other}"),
    }
}
