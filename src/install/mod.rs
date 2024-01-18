use async_ssh2_tokio::Client;
use log::error;

use crate::error::Error;
use crate::Result;

const WINDOWS_TARGET: &str = include_str!("target.bat");
const UNIX_TARGET: &str = include_str!("target.sh");

enum Os {
    Windows,
    UnixLike,
}

pub(crate) async fn installer(client: Client) -> Result<()> {
    let os = identify_os(&client).await?;
    let triple = get_triple(&client, os).await?;
    todo!("installing {}", triple);
}

async fn identify_os(client: &Client) -> Result<Os> {
    let result = client.execute("uname -a || ver").await?;

    if result.stdout.contains("Microsoft Windows") {
        Ok(Os::Windows)
    } else if result.stdout.contains("Linux")
        || result.stdout.contains("Darwin")
        || result.stdout.contains("BSD")
    {
        Ok(Os::UnixLike)
    } else {
        Err(Error::unknown_os(result.stdout, result.stderr))
    }
}

async fn get_triple(client: &Client, os: Os) -> Result<String> {
    let command = match os {
        Os::Windows => WINDOWS_TARGET,
        Os::UnixLike => UNIX_TARGET,
    };

    let result = client.execute(command).await?;

    if result.exit_status == 0 {
        Ok(result.stdout.replace('\n', ""))
    } else {
        error!("target command failed {}", result.stderr);
        Err(Error::command_failed(result.exit_status))
    }
}