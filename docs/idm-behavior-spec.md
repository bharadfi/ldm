# IDM v6.43 build 18 — observed download behavior (black-box, Windows)

Method: local HTTP server (`rangesrv.py`, logs every request) on 127.0.0.1:8765; IDM driven with
`IDMan.exe /n /d <url> /p <dir> /f <name>`. All files verified byte-for-byte by sampled pattern check.
Observed = seen in server log or on disk. Everything else is not claimed.

## Connection pattern (observed on 40 MB, 30 MB range-capable files)
1. First request: plain `GET` (no HEAD, no Range). Server answers 200 with full body.
2. Within ~20 ms IDM opens more connections with OPEN-ENDED ranges `bytes=N-<EOF>`:
   - segment A = [0, 233600) from the first connection (233600 bytes = first-connection head start,
     identical across runs and files, so it looks like a fixed constant),
   - remaining space split in four: starts at 233600, ~size/2, ~3/4 ...,
   - further splits halve the largest remaining segment, up to 8 concurrent connections.
3. First connection is aborted (client closes) once it has read its portion; later connections that reach the
   start of a neighbour's range are aborted too (client-side close, server sees broken pipe).
   Segment ends are often open-ended; the server-side `Content-Range` end is EOF, IDM just stops reading.
4. On a loopback fast server IDM keeps splitting (ranges of 13 KB - 25 KB appeared). Real-network behaviour untested.
5. Default connection count observed: 8.

## Request headers (first request)
`User-Agent: Mozilla/5.0 (Windows NT 6.1; Trident/7.0; rv:11.0) like Gecko`, `Accept: */*`,
`Accept-Encoding: identity`, `Accept-Language: en-IN` (system locale), `Accept-Charset: *`,
`Referer: <scheme://host:port/>` (origin root when none given). No `If-Range`, no `Connection` override seen.

## Servers without Range support (`Accept-Ranges` absent, Range ignored -> 200)
IDM still sends the first plain GET, then tries `Range: bytes=233600-` etc. (3 probes), sees 200 each time,
aborts them, and finishes with the single connection. File correct (10 MB).

## Redirects
302 to another path on same host: followed transparently; first request to final URL is plain GET, then
normal segmentation. Final filename taken from the original name given via /f (not tested without /f).

## Crash/resume (30 MB, throttled 150 KB/s per connection)
- Working data lives in `%APPDATA%\IDM\DwnlData\<user>\<name>_<id>\` :
  `<name>` (first segment), `<name>1..<name>8` (per-connection part files), `<name>_<id>.log` (plain text log).
- Killed IDMan.exe with Stop-Process -Force mid-download, relaunched the same URL:
  IDM again starts with plain GET (no If-Range/validators), then requests new ranges that start AFTER already
  saved bytes (e.g. `bytes=430208-` = 233600 + 196608 saved in part 1), and the file completed correctly.
- The `.log` file is plain text: URL, "User : anonymous", timestamps, and the raw response headers per connection.
- Queue state in `%APPDATA%\IDM\Scheduler\q_1.dt`.
- Parts stayed on disk after completion at time of observation (not checked later).

## Not captured (open)
- Content-Disposition filename without /f: IDM created a list entry (`cd_79`), sent one GET, wrote no file;
  probably waiting on a UI dialog. Not resolved.
- Real-network behaviour, proxies, auth, cookies, HLS, 416/5xx handling, bandwidth limiting, scheduler, categories.
- Browser integration path (native messaging, extension) at runtime.
