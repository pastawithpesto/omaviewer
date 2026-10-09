# Clipboard and filesystem implementation plan

Status: implementation added; see README and phone-test.md for verified behavior and remaining limits.

## Current code to reuse

- `host/src/control.rs` provides authenticated clipboard/files routes, wl-copy/wl-paste, ETags and upload finalization
- `android/src/clipboard.rs` bridges the clipboard and document picker through JNI
- `android/src/lib.rs` already runs transfer and clipboard work off the UI thread
- `shared/` contains clipboard conflict and retry rules

## Image clipboard

1. Add a typed clipboard descriptor with MIME type, revision, byte length and content identity. Keep existing text clients working
2. Add binary image reads/writes using explicit Wayland MIME types. Start with PNG and convert supported incoming images with bounded decoding
3. Inspect Android ClipDescription and URI items before text coercion. Never sync a content URI string as though it were the image
4. Read Android images through ContentResolver. Publish received images through a content provider backed by private cache files with temporary read access
5. Reuse foreground automatic synchronization, startup baselines, conflict handling and loop prevention for text and images. Keep clipboard image limits separate from file-transfer sizes
6. Desktop Paste uses the selected application's clipboard shortcut after sync completes. Terminal text Paste sends terminal bytes; image Paste offers upload and path insertion

Automatic sync does not promise clipboard access while Android has the app in the background. Keep cached clipboard content available while its URI is in use, then expire it under a bounded cache policy

## Integrated sharing

- Add Android Share to OmaViewer for one or multiple files and images, including shares received while the activity is already open
- Route Share, Attach and the Files view through one transfer queue with a visible destination and inline progress
- Preserve URI read grants for their valid lifetime. Copy to bounded staging storage when a grant cannot survive the required transfer lifetime
- Offer received files through Android Save or Share. Do not assume a content URI is a filesystem path
- Paste file clipboard items when supported by their provider. Keep Share and Attach available when the source app cannot expose files through the clipboard
- File transfers require an explicit send action. Copying a filename or changing the text clipboard does not upload a file

## System-wide file access

1. Run as the connected desktop user with their groups and filesystem permissions. Never rely on a client-supplied username or root service identity
2. Replace fixed-directory routes with structured paths and directory listings. Start at Home with navigation to root, mounts and a typed path
3. Support all permitted regular files, including hidden files and filenames that require escaping. Return opaque path identifiers if a filename cannot be represented as UTF-8
4. Let the kernel enforce access on the actual open/create operation. Report inaccessible directories, read-only mounts and insufficient space inline
5. Resolve links deliberately, display their destination and detect recursion loops. Use opened directory/file handles to avoid checking one target then operating on another
6. Refactor both the HTTP body parser and Android URI bridge to stream fixed-size buffers. Increasing FILE_MAX alone is not the fix
7. Track transfer ID, offset, source identity and chosen destination. Resume only when source and partial destination still match; otherwise offer restart
8. Stage uploads beside their final destination where permitted, then finalize after completion. Handle writable existing files in non-writable directories explicitly because atomic replacement may be unavailable
9. Confirm replace, keep both or skip inline. Preserve existing file metadata when appropriate; derive new file permissions from the desktop user's policy
10. Traverse folder trees as individual entries with a bounded queue and progress. Support cancellation, cleanup and explicit retention of resumable partial files

System-wide means paths available to this desktop user, including outside Home. Special device files, sockets and pipes are not ordinary file transfers

## Acceptance checks

- Copy text and PNG images both ways, then paste into dedicated test apps without clipboard loops
- Share one file and multiple files with the app open and closed
- Transfer a file larger than 100 MiB without memory growing with file size
- Upload/download outside Home in permitted paths and show genuine permission failures
- Test interrupted upload/download resume, changed source, cancellation, full disk and overwrite choices
- Test spaces, Unicode, quotes, hidden files, symlinks and nested folders
- Terminal insertion quotes paths correctly and never executes a command automatically

## Platform references

- [Android clipboard types and content URIs](https://developer.android.com/develop/ui/views/touch-and-input/copy-paste)
- [Android file sharing and temporary URI access](https://developer.android.com/training/secure-file-sharing/share-file)
