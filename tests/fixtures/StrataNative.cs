// Win32 helpers for the fixture scripts, compiled with Add-Type.
//
// Windows PowerShell 5.1 runs on .NET Framework, whose path normalization
// strips trailing dots and spaces and rejects reserved device names and
// unpaired surrogates. These wrappers call the W APIs directly with \\?\
// paths so such names reach NTFS verbatim, and walk volumes the same way to
// produce the expected-output file the MFT scan is compared against.

using System;
using System.Collections.Generic;
using System.ComponentModel;
using System.IO;
using System.Runtime.InteropServices;
using Microsoft.Win32.SafeHandles;

public static class StrataNative
{
    const uint GENERIC_WRITE = 0x40000000;
    const uint FILE_READ_ATTRIBUTES = 0x80;
    const uint SHARE_ALL = 0x7;
    const uint CREATE_ALWAYS = 2;
    const uint OPEN_EXISTING = 3;
    const uint FILE_FLAG_BACKUP_SEMANTICS = 0x02000000;
    const uint FILE_FLAG_OPEN_REPARSE_POINT = 0x00200000;
    const int ERROR_ALREADY_EXISTS = 183;
    const uint FILE_ATTRIBUTE_DIRECTORY = 0x10;
    const uint FILE_ATTRIBUTE_REPARSE_POINT = 0x400;

    [DllImport("kernel32.dll", CharSet = CharSet.Unicode, SetLastError = true)]
    static extern bool CreateDirectoryW(string path, IntPtr security);

    [DllImport("kernel32.dll", CharSet = CharSet.Unicode, SetLastError = true)]
    static extern SafeFileHandle CreateFileW(string path, uint access, uint share, IntPtr security,
        uint disposition, uint flags, IntPtr template);

    [DllImport("kernel32.dll", CharSet = CharSet.Unicode, SetLastError = true)]
    static extern bool CreateHardLinkW(string newName, string existing, IntPtr security);

    [DllImport("kernel32.dll", CharSet = CharSet.Unicode, SetLastError = true)]
    static extern uint GetCompressedFileSizeW(string path, out uint high);

    [StructLayout(LayoutKind.Sequential)]
    struct FILE_STANDARD_INFO
    {
        public long AllocationSize;
        public long EndOfFile;
        public uint NumberOfLinks;
        public byte DeletePending;
        public byte Directory;
    }

    [DllImport("kernel32.dll", SetLastError = true)]
    static extern bool GetFileInformationByHandleEx(SafeFileHandle h, int infoClass,
        out FILE_STANDARD_INFO info, uint size);

    // FILETIME is two DWORDs (4-byte aligned); Pack = 4 keeps the 64-bit
    // fields at their native offsets.
    [StructLayout(LayoutKind.Sequential, CharSet = CharSet.Unicode, Pack = 4)]
    struct WIN32_FIND_DATAW
    {
        public uint dwFileAttributes;
        public long ftCreationTime;
        public long ftLastAccessTime;
        public long ftLastWriteTime;
        public uint nFileSizeHigh;
        public uint nFileSizeLow;
        public uint dwReserved0;
        public uint dwReserved1;
        [MarshalAs(UnmanagedType.ByValTStr, SizeConst = 260)] public string cFileName;
        [MarshalAs(UnmanagedType.ByValTStr, SizeConst = 14)] public string cAlternateFileName;
    }

    [DllImport("kernel32.dll", CharSet = CharSet.Unicode, SetLastError = true)]
    static extern IntPtr FindFirstFileExW(string path, int infoLevel, out WIN32_FIND_DATAW data,
        int searchOp, IntPtr filter, int flags);

    [DllImport("kernel32.dll", CharSet = CharSet.Unicode, SetLastError = true)]
    static extern bool FindNextFileW(IntPtr h, out WIN32_FIND_DATAW data);

    [StructLayout(LayoutKind.Sequential, CharSet = CharSet.Unicode)]
    struct WIN32_FIND_STREAM_DATA
    {
        public long StreamSize;
        [MarshalAs(UnmanagedType.ByValTStr, SizeConst = 296)] public string cStreamName;
    }

    [DllImport("kernel32.dll", CharSet = CharSet.Unicode, SetLastError = true)]
    static extern IntPtr FindFirstStreamW(string path, int level, out WIN32_FIND_STREAM_DATA data, uint flags);

    [DllImport("kernel32.dll", CharSet = CharSet.Unicode, SetLastError = true)]
    static extern bool FindNextStreamW(IntPtr h, out WIN32_FIND_STREAM_DATA data);

    [DllImport("kernel32.dll", SetLastError = true)]
    static extern bool FindClose(IntPtr h);

    static readonly IntPtr Invalid = new IntPtr(-1);

    static Exception Fail(string what)
    {
        int e = Marshal.GetLastWin32Error();
        return new Win32Exception(e, what + ": " + new Win32Exception(e).Message);
    }

    /// <summary>Creates a directory; an existing one is fine.</summary>
    public static void Dir(string path)
    {
        if (!CreateDirectoryW(path, IntPtr.Zero) && Marshal.GetLastWin32Error() != ERROR_ALREADY_EXISTS)
            throw Fail(path);
    }

    /// <summary>Creates or truncates a file and writes data (path may name a stream).</summary>
    public static void File(string path, byte[] data)
    {
        using (SafeFileHandle h = CreateFileW(path, GENERIC_WRITE, 0, IntPtr.Zero, CREATE_ALWAYS, 0x80, IntPtr.Zero))
        {
            if (h.IsInvalid) throw Fail(path);
            using (FileStream fs = new FileStream(h, FileAccess.Write))
                fs.Write(data, 0, data.Length);
        }
    }

    /// <summary>Creates a hardlink newName -> existing.</summary>
    public static void HardLink(string newName, string existing)
    {
        if (!CreateHardLinkW(newName, existing, IntPtr.Zero)) throw Fail(newName);
    }

    /// <summary>One entry of the expected-output walk.</summary>
    public sealed class Entry
    {
        public string path;
        public string kind;
        public long logical;
        public long allocation_size;
        public long compressed_size;
        public uint attributes;
        public string reparse_tag;
        public uint link_count;
        public List<KeyValuePair<string, long>> streams = new List<KeyValuePair<string, long>>();
        public string error;
    }

    /// <summary>
    /// Walks root (e.g. X:\) without following reparse points and returns
    /// one entry per path, with sizes from the Win32 file APIs.
    /// </summary>
    public static List<Entry> Walk(string root)
    {
        string volume = root.TrimEnd('\\');
        List<Entry> result = new List<Entry>();
        Stack<string> dirs = new Stack<string>();
        dirs.Push("");
        while (dirs.Count > 0)
        {
            string rel = dirs.Pop();
            WIN32_FIND_DATAW fd;
            IntPtr h = FindFirstFileExW(@"\\?\" + volume + rel + @"\*", 1, out fd, 0, IntPtr.Zero, 0);
            if (h == Invalid) continue;
            try
            {
                do
                {
                    if (fd.cFileName == "." || fd.cFileName == "..") continue;
                    string childRel = rel + "\\" + fd.cFileName;
                    Entry e = Describe(@"\\?\" + volume + childRel, childRel, fd);
                    result.Add(e);
                    bool isDir = (fd.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY) != 0;
                    bool isReparse = (fd.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT) != 0;
                    if (isDir && !isReparse) dirs.Push(childRel);
                } while (FindNextFileW(h, out fd));
            }
            finally { FindClose(h); }
        }
        return result;
    }

    static Entry Describe(string full, string rel, WIN32_FIND_DATAW fd)
    {
        Entry e = new Entry();
        e.path = rel;
        e.attributes = fd.dwFileAttributes;
        bool isDir = (fd.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY) != 0;
        e.kind = isDir ? "dir" : "file";
        e.logical = isDir ? 0 : ((long)fd.nFileSizeHigh << 32) | fd.nFileSizeLow;
        if ((fd.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT) != 0)
            e.reparse_tag = "0x" + fd.dwReserved0.ToString("X8");
        using (SafeFileHandle h = CreateFileW(full, FILE_READ_ATTRIBUTES, SHARE_ALL, IntPtr.Zero, OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT, IntPtr.Zero))
        {
            FILE_STANDARD_INFO si;
            if (h.IsInvalid)
                e.error = new Win32Exception(Marshal.GetLastWin32Error()).Message;
            else if (GetFileInformationByHandleEx(h, 1, out si, (uint)Marshal.SizeOf(typeof(FILE_STANDARD_INFO))))
            {
                e.allocation_size = si.AllocationSize;
                e.link_count = si.NumberOfLinks;
            }
        }
        if (!isDir)
        {
            uint high;
            uint low = GetCompressedFileSizeW(full, out high);
            if (low != 0xFFFFFFFF || Marshal.GetLastWin32Error() == 0)
                e.compressed_size = ((long)high << 32) | low;
            WIN32_FIND_STREAM_DATA sd;
            IntPtr sh = FindFirstStreamW(full, 0, out sd, 0);
            if (sh != Invalid)
            {
                try
                {
                    do
                    {
                        if (sd.cStreamName != "::$DATA")
                            e.streams.Add(new KeyValuePair<string, long>(sd.cStreamName, sd.StreamSize));
                    } while (FindNextStreamW(sh, out sd));
                }
                finally { FindClose(sh); }
            }
        }
        return e;
    }
}
