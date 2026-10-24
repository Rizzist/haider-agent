# SSH terminal setup

The daemon bounds DNS and TCP connection establishment to 10 seconds. SSH
negotiation and authentication share a further 30-second deadline. The entire
PTY setup has a 50-second deadline, including waiting for the profile's session
lock, reconnecting, and opening the channel. Reconnects do not reset that total.
The remaining time inside the client's 60-second request budget allows the
daemon to deliver a typed `ssh_timeout` refusal (CLI exit 76). Terminal state
and the pending setup permit are released when setup expires or is cancelled.

Ctrl+] closes the CLI terminal during setup and after connection, including
the legacy terminal encoding 0x1D. Ctrl+C also cancels CLI setup with a normal
close. Once the daemon reports the shell running, Ctrl+C is forwarded to it.
The connected human shell has no setup deadline.

The discarded-input feedback marker expires after 10 seconds or accepted
replacement input, including the next accepted submission. Paints, passive
mouse reports, key releases, and refused confirmations do not extend it.
