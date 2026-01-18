# Music163bot-Rust v1.1.0 Release Notes

## Summary

v1.1.0 introduces a new smart storage system that optimizes the temporary file workflow (download -> metadata -> upload). It offers three storage modes to improve performance, reduce disk usage, and limit disk I/O.

## New Features

- Smart storage system with three modes:
  - **disk**: traditional file-based storage (stable, low memory usage)
  - **memory**: full in-memory processing (faster, minimal disk I/O)
  - **hybrid**: smart selection based on file size and available memory (recommended)
- Automatic mode selection and fallback to disk when memory is insufficient
- MP3 and FLAC metadata processing now work in both disk and memory modes

## Performance Benefits

- Reduced disk I/O during downloads and metadata processing
- Faster processing for small files with in-memory buffering
- Lower SSD/HDD wear in memory or hybrid modes
- Flexible operation on memory-rich or disk-limited servers

## Configuration

Add these settings under the `[download]` section:

```ini
[download]
storage_mode = hybrid
memory_threshold = 100
memory_buffer = 100
```

- `storage_mode`: `disk`, `memory`, or `hybrid`
- `memory_threshold`: file size threshold in MB for hybrid mode (default: 100)
- `memory_buffer`: safety buffer in MB for available memory checks (default: 100)

## Backward Compatibility

- If no storage settings are configured, the bot defaults to `disk` mode (v1.0.0 behavior)
- Existing configurations remain valid without changes

## Technical Notes

- MP3: `id3` supports full in-memory tagging via `read_from` and `write_to`
- FLAC: `metaflac` supports in-memory metadata reconstruction; audio frames are preserved
- Both formats work across disk, memory, and hybrid modes
