# cccp
A network file transfer utility inspired by [bbcp](https://github.com/eeertekin/bbcp)
with a focus on cross-platform support, a high level of configurability, security, 
efficiency, and of course speed.

## IoSpec
IoSpecs define local or remote files with connection details for ssh. If you have 
used scp, you will be familiar with the syntax. 
- \[user@\]\[host:{port:}\]file
- If no user is set for a remote host, the current user is used
- If no port is provided, port 22 is used
## Ciphers
A variety of ciphers are supported. Using at least CHACHA8 is recommended as the 
performance impact is minimal.
- NONE
- CHACHA8
- CHAHA20
- AES128
- AES192
- AES256
## Firewall
At least three ports are required for cccp's use, not including SSH. The
first two ports are TCP sockets used for control. Any remaining ports (at least one)
are used for data transfer on UDP.
## Installer
cccp can install itself on a remote host as long as that host supports SSH & SFTP.
- The installation location should be in PATH. Alternatively, pass the absolute path to the `command` transfer argument
- `/usr/bin/cccp` is a good choice on Linux and BSD
- `/usr/local/bin/cccp` is a good choice on macOS
- On Windows you could install to `C:\Windows` or `C:\Windows\System32` but this is not recommended
```
Usage: cccp[.exe] install [OPTIONS] <DESTINATION>
  
Arguments:  
<DESTINATION> IoSpec for the remote host & install location  
  
Options:  
-l, --log-level <LOG_LEVEL>          Log level [debug, info, warn, error] [default: warn]  
-c, --custom-binary <CUSTOM_BINARY>  Use a custom binary instead of downloading from Github  
-h, --help                           Print help  
-V, --version                        Print version  
```
## Transfer Usage
```
Usage: cccp[.exe] [OPTIONS] <SOURCE> <DESTINATION>  
  
Arguments:  
<SOURCE>       Source IoSpec (InSpec)  
<DESTINATION>  Destination IoSpec (OutSpec)  
  
Options:  
-s, --start-port <START_PORT>                First port to use [default: 50000]  
-e, --end-port <END_PORT>                    Last port to use [default: 50009]  
-t, --threads <THREADS>                      Parallel data streams [default: 8]  
-l, --log-level <LOG_LEVEL>                  Log level [debug, info, warn, error] [default: warn]  
-r, --rate <RATE>                            Data send rate [b, kb, mb, gb, tb] [default: 1mb]  
-m, --max <MAX>                              Maximum concurrent transfers [default: 100]  
-c, --control-crypto <CONTROL_CRYPTO>        Encrypt the control stream [default: AES256]  
-S, --stream-crypto <STREAM_CRYPTO>          Encrypt the data stream [default: CHACHA20]  
-T, --receive-timeout <RECEIVE_TIMEOUT>      Receive timeout in MS [default: 5000]  
-j, --job-limit <JOB_LIMIT>                  Limit for concurrent jobs [default: 1000]  
-i, --requeue-interval <REQUEUE_INTERVAL>    Requeue interval in MS [default: 1000]  
-M, --max-retries <MAX_RETRIES>              Maximum number of send/receive retries [default: 10]  
-E, --command <COMMAND>                      Command to execute cccp [default: cccp]  
-p, --progress-interval <PROGRESS_INTERVAL>  How often to print progress in MS [default: 1000]  
-v, --verify                                 Verify integrity of transfers using blake3  
-o, --overwrite                              Overwrite existing files  
-R, --recursive                              Include subdirectories recursively  
-f, --force                                  Do not check destination's available storage  
-d, --directory                              Forces the destination to be a directory  
-L, --log-file <LOG_FILE>                    Log to a file (default: stderr / local only)  
-h, --help                                   Print help  
-V, --version                                Print version
```  
## Roadmap
- [ ] Add tests
- [ ] Optimize dependency tree
- [ ] Add further precompiled targets
- [ ] Add chained ciphers
- [ ] Add a command to check for updates