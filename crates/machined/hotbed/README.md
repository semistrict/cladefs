# hotbed's API, copied

`proto/hotbed/v1/hotbed.proto` is hotbed's own API, copied unchanged from
[semistrict/hotbed](https://github.com/semistrict/hotbed) (MIT, `LICENSE` here) at commit
`5a9531ef01729ea5243bbfcbda210af18a3622f2` (2026-11-26), for machined's hotbed node
(`src/hotbed`): its build (`build.rs`) generates the messages alone, and reads its descriptors to
know WorkspaceService's calls.

Keep it a copy: to follow hotbed, copy the file again from a newer commit and update the commit
here. It sits outside `proto/`, so that buf's lint and `go/compat.sh`, which hold machined's own
API, never see it.
