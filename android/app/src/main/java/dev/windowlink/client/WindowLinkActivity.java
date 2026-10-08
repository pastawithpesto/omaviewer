package dev.windowlink.client;

import android.app.NativeActivity;
import android.view.KeyEvent;
import android.content.Intent;
import android.content.ClipData;
import android.content.ClipboardManager;
import android.os.Bundle;
import android.os.ParcelFileDescriptor;
import android.provider.DocumentsContract;
import org.json.JSONArray;
import org.json.JSONObject;
import java.io.File;
import java.io.FileOutputStream;
import java.util.ArrayDeque;
import java.util.ArrayList;
import java.security.MessageDigest;
import android.database.Cursor;
import android.net.Uri;
import android.provider.OpenableColumns;

import java.io.ByteArrayOutputStream;
import java.io.IOException;
import java.io.InputStream;
import java.io.OutputStream;

/** Small SAF bridge; file contents and network transfer remain in Rust. */
public final class WindowLinkActivity extends NativeActivity {
    private native boolean nativeWindowLinkHardwareKey(int keyCode, int deviceId, int action);

    @Override
    public boolean dispatchKeyEvent(KeyEvent event) {
        // Real hardware only: phone IME composition follows NativeActivity.
        if (event.getDevice() != null && !event.getDevice().isVirtual() && (event.getFlags() & KeyEvent.FLAG_SOFT_KEYBOARD) == 0
                && nativeWindowLinkHardwareKey(event.getKeyCode(), event.getDeviceId(), event.getAction())) {
            return true;
        }
        return super.dispatchKeyEvent(event);
    }

    private final ArrayDeque<String> transferResults = new ArrayDeque<>();
    private final ArrayList<String> sharedUris = new ArrayList<>();
    private final java.util.HashMap<Integer,String[]> pickerContexts=new java.util.HashMap<>();
    private int nextTransferRequest=6000;
    private static final int TRANSFER_UPLOAD = 5101, TRANSFER_SAVE = 5102, TREE_UPLOAD = 5103, TREE_SAVE = 5104;

    @Override protected void onCreate(Bundle savedInstanceState) {
        super.onCreate(savedInstanceState);
        acceptShare(getIntent());
    }
    @Override protected void onNewIntent(Intent intent) {
        super.onNewIntent(intent);
        setIntent(intent);
        acceptShare(intent);
    }
    private synchronized void acceptShare(Intent intent) {
        if (intent == null) return;
        String action = intent.getAction();
        if (!Intent.ACTION_SEND.equals(action) && !Intent.ACTION_SEND_MULTIPLE.equals(action)) return;
        ArrayList<Uri> incoming = new ArrayList<>();
        if (Intent.ACTION_SEND_MULTIPLE.equals(action)) {
            ArrayList<Uri> items = intent.getParcelableArrayListExtra(Intent.EXTRA_STREAM);
            if (items != null) incoming.addAll(items);
        } else {
            Uri item = intent.getParcelableExtra(Intent.EXTRA_STREAM);
            if (item != null) incoming.add(item);
        }
        if (intent.getClipData() != null) for (int i = 0; i < intent.getClipData().getItemCount(); i++) {
            Uri item = intent.getClipData().getItemAt(i).getUri();
            if (item != null && !incoming.contains(item)) incoming.add(item);
        }
        for (Uri uri : incoming) {
            if (!"content".equals(uri.getScheme())) continue;
            try { getContentResolver().takePersistableUriPermission(uri, Intent.FLAG_GRANT_READ_URI_PERMISSION); } catch (SecurityException ignored) { }
            if (!sharedUris.contains(uri.toString()) && sharedUris.size() < 128) sharedUris.add(uri.toString());
        }
        // Consume the intent once so activity recreation cannot resend a file.
        intent.setAction(null);
    }
    public synchronized String takeWindowLinkSharedUris() {
        String result = new JSONArray(sharedUris).toString(); sharedUris.clear(); return result;
    }
    public void startWindowLinkTransferPicker(String kind, String suggestedName, String generation) {
        final boolean tree = kind.startsWith("folder");
        final boolean save = kind.equals("save") || kind.equals("folder_save");
        Intent intent = new Intent(tree ? Intent.ACTION_OPEN_DOCUMENT_TREE : save ? Intent.ACTION_CREATE_DOCUMENT : Intent.ACTION_OPEN_DOCUMENT);
        intent.addFlags(Intent.FLAG_GRANT_READ_URI_PERMISSION | Intent.FLAG_GRANT_WRITE_URI_PERMISSION | Intent.FLAG_GRANT_PERSISTABLE_URI_PERMISSION);
        if (!tree) { intent.addCategory(Intent.CATEGORY_OPENABLE); intent.setType("*/*"); }
        if (!tree && save) intent.putExtra(Intent.EXTRA_TITLE, suggestedName);
        if (!tree && !save) intent.putExtra(Intent.EXTRA_ALLOW_MULTIPLE, true);
        final int request; synchronized(this) {request=nextTransferRequest++; if(nextTransferRequest>65000)nextTransferRequest=6000;pickerContexts.put(request,new String[]{kind,generation});}
        startActivityForResult(intent, request);
    }
    public synchronized String takeWindowLinkTransferResult() { return transferResults.pollFirst(); }
    public int openWindowLinkDocument(String uriText, String mode) throws IOException {
        ParcelFileDescriptor file = getContentResolver().openFileDescriptor(Uri.parse(uriText), mode);
        if (file == null) throw new IOException("Could not open document");
        try { return file.detachFd(); } finally { file.close(); }
    }
    public String windowLinkCacheDirectory() { return getCacheDir().getAbsolutePath(); }
    public String windowLinkDocumentMetadata(String uriText) throws Exception {
        Uri uri = Uri.parse(uriText); JSONObject result = new JSONObject();
        result.put("uri", uriText); result.put("name", windowLinkDisplayName(uriText));
        result.put("mime", getContentResolver().getType(uri));
        boolean persisted = false;
        for (android.content.UriPermission permission : getContentResolver().getPersistedUriPermissions()) {
            if (!permission.isReadPermission()) continue;
            Uri grant = permission.getUri();
            boolean sameTree = DocumentsContract.isTreeUri(uri) && DocumentsContract.isTreeUri(grant)
                && java.util.Objects.equals(uri.getAuthority(), grant.getAuthority())
                && DocumentsContract.getTreeDocumentId(uri).equals(DocumentsContract.getTreeDocumentId(grant));
            if (grant.equals(uri) || sameTree) { persisted = true; break; }
        }
        result.put("persisted", persisted);
        try (Cursor cursor = getContentResolver().query(uri, null, null, null, null)) {
            if (cursor != null && cursor.moveToFirst()) {
                int size = cursor.getColumnIndex(OpenableColumns.SIZE);
                int modified = cursor.getColumnIndex(DocumentsContract.Document.COLUMN_LAST_MODIFIED);
                if (size >= 0 && !cursor.isNull(size)) result.put("size", cursor.getLong(size));
                if (modified >= 0 && !cursor.isNull(modified)) result.put("modified", cursor.getLong(modified));
            }
        }
        return result.toString();
    }
    public String windowLinkTreeEntries(String uriText, int offset) throws Exception {
        Uri tree = Uri.parse(uriText);
        String id = DocumentsContract.isDocumentUri(this, tree) ? DocumentsContract.getDocumentId(tree) : DocumentsContract.getTreeDocumentId(tree);
        Uri children = DocumentsContract.buildChildDocumentsUriUsingTree(tree, id);
        JSONArray entries = new JSONArray(); int seen = 0; boolean more = false;
        try (Cursor cursor = getContentResolver().query(children, null, null, null, null)) {
            if (cursor == null) throw new IOException("Could not list selected folder");
            while (cursor.moveToNext()) {
                if (seen++ < offset) continue;
                if (entries.length() == 200) { more = true; break; }
                String childId = cursor.getString(cursor.getColumnIndexOrThrow(DocumentsContract.Document.COLUMN_DOCUMENT_ID));
                String name = cursor.getString(cursor.getColumnIndexOrThrow(DocumentsContract.Document.COLUMN_DISPLAY_NAME));
                String mime = cursor.getString(cursor.getColumnIndexOrThrow(DocumentsContract.Document.COLUMN_MIME_TYPE));
                JSONObject item = new JSONObject(); item.put("uri", DocumentsContract.buildDocumentUriUsingTree(tree, childId).toString()); item.put("name", name); item.put("directory", DocumentsContract.Document.MIME_TYPE_DIR.equals(mime)); entries.put(item);
            }
        }
        return new JSONObject().put("entries", entries).put("next_cursor", more ? offset + entries.length() : JSONObject.NULL).toString();
    }
    public String createWindowLinkDocument(String parentText, String name, boolean directory) throws Exception {
        Uri parent = Uri.parse(parentText);
        if (!DocumentsContract.isDocumentUri(this, parent)) parent = DocumentsContract.buildDocumentUriUsingTree(parent, DocumentsContract.getTreeDocumentId(parent));
        String mime = directory ? DocumentsContract.Document.MIME_TYPE_DIR : "application/octet-stream";
        Uri created = DocumentsContract.createDocument(getContentResolver(), parent, mime, name);
        if (created == null) throw new IOException("Could not create document"); return created.toString();
    }
    public String windowLinkClipboardDescriptor() throws Exception {
        ClipboardManager manager = (ClipboardManager)getSystemService(CLIPBOARD_SERVICE);
        ClipData clip = manager.getPrimaryClip(); JSONObject result = new JSONObject();
        if (clip == null || clip.getItemCount() == 0) return result.put("mime", JSONObject.NULL).toString();
        ClipData.Item item = clip.getItemAt(0); Uri uri = item.getUri();
        if (uri != null) {
            String mime = getContentResolver().getType(uri);
            if (mime != null && mime.startsWith("image/")) return result.put("mime", mime).put("uri", uri.toString()).toString();
            // A file URI is not clipboard text and copying it never uploads it.
            return result.put("mime", JSONObject.NULL).toString();
        }
        CharSequence text = item.getText();
        return text == null ? result.put("mime", JSONObject.NULL).toString() : result.put("mime", "text/plain; charset=utf-8").put("text", text.toString()).toString();
    }
    public synchronized void setWindowLinkImageClipboard(byte[] bytes) throws Exception {
        if (bytes.length > 32 * 1024 * 1024) throw new IOException("Clipboard image exceeds 32 MiB");
        File directory = new File(getCacheDir(), "windowlink-share"); if (!directory.exists() && !directory.mkdirs()) throw new IOException("Could not create clipboard cache");
        StringBuilder digest = new StringBuilder(); for (byte b : MessageDigest.getInstance("SHA-256").digest(bytes)) digest.append(String.format("%02x", b & 255));
        File image = new File(directory, "clip-" + digest + ".png");
        ClipboardManager manager = (ClipboardManager)getSystemService(CLIPBOARD_SERVICE);
        ClipData current = manager.getPrimaryClip();
        String currentName = current != null && current.getItemCount() > 0 && current.getItemAt(0).getUri() != null ? current.getItemAt(0).getUri().getLastPathSegment() : "";
        File[] cached = directory.listFiles(); long kept = 0;
        if (cached != null) for (File file : cached) {
            if (!file.getName().equals(currentName) && System.currentTimeMillis() - file.lastModified() > 24L*60*60*1000) file.delete();
            if (file.exists()) kept += file.length();
        }
        if (!image.exists()) {
            if (kept + bytes.length > 256L*1024*1024) throw new IOException("Clipboard cache is full; retained image URIs expire after 24 hours");
            File temporary = File.createTempFile("image-", ".tmp", directory);
            try {
                try (FileOutputStream output = new FileOutputStream(temporary)) { output.write(bytes); output.getFD().sync(); }
                if (!temporary.renameTo(image)) throw new IOException("Could not publish clipboard image");
            } finally { temporary.delete(); }
        }
        Uri uri = new Uri.Builder().scheme("content").authority(getPackageName() + ".share").appendPath(image.getName()).build();
        manager.setPrimaryClip(ClipData.newUri(getContentResolver(), "OmaLink image", uri));
    }

    private static final int PICK_UPLOAD = 4101;
    private static final int PICK_SAVE = 4102;
    private static final int MAX_FILE_BYTES = 100 * 1024 * 1024;
    private String pendingFileResult;

    public void startWindowLinkFilePicker(boolean save, String suggestedName) {
        Intent intent = new Intent(save ? Intent.ACTION_CREATE_DOCUMENT : Intent.ACTION_OPEN_DOCUMENT);
        intent.addCategory(Intent.CATEGORY_OPENABLE);
        intent.setType("*/*");
        int requestCode = save ? PICK_SAVE : PICK_UPLOAD;
        if (save) intent.putExtra(Intent.EXTRA_TITLE, suggestedName);
        startActivityForResult(intent, requestCode);
    }

    @Override
    protected void onActivityResult(int requestCode, int resultCode, Intent data) {
        super.onActivityResult(requestCode, resultCode, data);
        final String[] picker; synchronized(this) {picker=pickerContexts.remove(requestCode);}
        if (picker != null) {
            try {
                JSONObject result = new JSONObject();
                result.put("kind", picker[0]); result.put("generation",picker[1]); JSONArray uris = new JSONArray();
                if (resultCode == RESULT_OK && data != null) {
                    if (data.getClipData() != null) for (int i=0;i<data.getClipData().getItemCount();i++) { Uri uri=data.getClipData().getItemAt(i).getUri(); if(uri!=null)uris.put(uri.toString()); }
                    else if (data.getData() != null) uris.put(data.getData().toString());
                    int grants=data.getFlags() & (Intent.FLAG_GRANT_READ_URI_PERMISSION | Intent.FLAG_GRANT_WRITE_URI_PERMISSION);
                    for(int i=0;i<uris.length();i++) try {getContentResolver().takePersistableUriPermission(Uri.parse(uris.getString(i)),grants);}catch(SecurityException ignored){}
                }
                result.put("uris", uris); synchronized(this){transferResults.addLast(result.toString());}
            } catch(Exception ignored) { }
            return;
        }
        if (requestCode != PICK_UPLOAD && requestCode != PICK_SAVE) return;
        if (resultCode != RESULT_OK || data == null || data.getData() == null) {
            synchronized (this) { pendingFileResult = "cancel\n"; }
            return;
        }
        Uri uri = data.getData();
        int grants = data.getFlags() & (Intent.FLAG_GRANT_READ_URI_PERMISSION | Intent.FLAG_GRANT_WRITE_URI_PERMISSION);
        try { getContentResolver().takePersistableUriPermission(uri, grants); } catch (SecurityException ignored) { }
        synchronized (this) {
            pendingFileResult = (requestCode == PICK_UPLOAD ? "upload\n" : "save\n") + uri;
        }
    }

    public synchronized String takeWindowLinkFileResult() {
        String result = pendingFileResult;
        pendingFileResult = null;
        return result;
    }

    public String windowLinkDisplayName(String uriText) {
        Uri uri = Uri.parse(uriText);
        try (Cursor cursor = getContentResolver().query(uri, null, null, null, null)) {
            if (cursor != null && cursor.moveToFirst()) {
                int column = cursor.getColumnIndex(OpenableColumns.DISPLAY_NAME);
                if (column >= 0) return cursor.getString(column);
            }
        }
        return uri.getLastPathSegment() == null ? "upload.bin" : uri.getLastPathSegment();
    }

    public byte[] readWindowLinkFile(String uriText) throws IOException {
        try (InputStream input = getContentResolver().openInputStream(Uri.parse(uriText));
             ByteArrayOutputStream output = new ByteArrayOutputStream()) {
            if (input == null) throw new IOException("Could not open selected file");
            byte[] buffer = new byte[64 * 1024];
            int total = 0;
            int count;
            while ((count = input.read(buffer)) != -1) {
                total += count;
                if (total > MAX_FILE_BYTES) throw new IOException("File exceeds 100 MiB");
                output.write(buffer, 0, count);
            }
            return output.toByteArray();
        }
    }

    public void writeWindowLinkFile(String uriText, byte[] contents) throws IOException {
        if (contents.length > MAX_FILE_BYTES) throw new IOException("File exceeds 100 MiB");
        try (OutputStream output = getContentResolver().openOutputStream(Uri.parse(uriText), "w")) {
            if (output == null) throw new IOException("Could not create selected file");
            output.write(contents);
            output.flush();
        }
    }
}
