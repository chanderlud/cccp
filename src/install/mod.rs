use std::io::{Error as IoError, ErrorKind as IoErrorKind, Read};
use std::path::PathBuf;

use async_compression::futures::bufread::GzipDecoder;
use async_ssh2_tokio::Client;
use futures::{AsyncReadExt, TryStreamExt};
use log::{debug, error, info, warn};
use russh_sftp::client::SftpSession;
use semver::Version;
use tar::Archive;
use tokio::fs::File;
use tokio::io::AsyncWriteExt;

use crate::error::Error;
use crate::Result;

const WINDOWS_TARGET: &str = include_str!("target.bat");
const UNIX_TARGET: &str = include_str!("target.sh");
const VERSION: &str = env!("CARGO_PKG_VERSION");

enum Os {
    Windows,
    UnixLike,
}

pub(crate) async fn installer(
    client: Client,
    destination: PathBuf,
    custom_binary: Option<PathBuf>,
) -> Result<()> {
    if let Some(file_path) = custom_binary {
        debug!("using custom binary {}", file_path.display());

        let mut file = File::open(file_path).await?;
        let mut content = Vec::new();
        tokio::io::AsyncReadExt::read_to_end(&mut file, &mut content).await?;

        transfer_binary(&client, &content, destination).await?;

        return Ok(());
    }

    let os = identify_os(&client).await?; // identify the remote os
    let triple = get_triple(&client, &os).await?; // get the target triple

    // initialize octocrab
    let octocrab = octocrab::instance();

    // get the latest release
    let release = octocrab
        .repos("chanderlud", "cccp")
        .releases()
        .get_latest()
        .await?;

    info!("installing cccp-{}-{}", release.tag_name, triple);

    // check if the local version is out of date
    if Version::parse(VERSION).unwrap()
        < Version::parse(release.tag_name.strip_prefix('v').unwrap()).unwrap()
    {
        warn!(
            "the local version of cccp is out of date [v{} < {}]",
            VERSION, release.tag_name
        );
    }

    for asset in &release.assets {
        if asset.name.contains(&triple) {
            debug!("fetching asset {}", asset.name);

            let response = octocrab._get(asset.browser_download_url.as_str()).await?;

            // convert the response body into a stream of bytes
            let body_stream = response
                .into_body()
                .map_err(|err| IoError::new(IoErrorKind::Other, err));

            let reader = body_stream.into_async_read(); // stream -> reader
            let mut decoder = GzipDecoder::new(reader); // reader -> decoder

            let mut buffer = Vec::new();
            decoder.read_to_end(&mut buffer).await?; // decode response into buffer
            debug!("decompressed archive to size {}", buffer.len());

            let mut archive = Archive::new(buffer.as_slice()); // create archive from buffer
            let mut file = archive.entries()?.next().ok_or(Error::file_not_found())??; // get the file

            let mut content = Vec::new();
            file.read_to_end(&mut content)?; // read the file into a buffer
            debug!("got file with size {}", content.len());

            // install the binary to the destination
            transfer_binary(&client, &content, destination).await?;

            return Ok(());
        }
    }

    Err(Error::no_suitable_release())
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

async fn get_triple(client: &Client, os: &Os) -> Result<String> {
    let command = match os {
        Os::Windows => WINDOWS_TARGET,
        Os::UnixLike => UNIX_TARGET,
    };

    let result = client.execute(command).await?;

    if result.exit_status == 0 {
        Ok(result.stdout.replace(['\n', '\r'], ""))
    } else {
        error!("target command failed {}", result.stderr);
        Err(Error::command_failed(result.exit_status))
    }
}

async fn transfer_binary(client: &Client, content: &[u8], destination: PathBuf) -> Result<()> {
    let channel = client.get_channel().await?; // get a channel
    channel.request_subsystem(true, "sftp").await?; // start sftp
    let sftp = SftpSession::new(channel.into_stream()).await?; // create sftp session
    debug!("created sftp session");

    // create the destination file
    let mut file = sftp.create(destination.to_string_lossy()).await?;
    file.write_all(content).await?; // write the file to the destination
    debug!("wrote file to {}", destination.display());

    Ok(())
}
