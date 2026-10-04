pub const c = @cImport({
    @cInclude("alpm.h");
    @cInclude("curl/curl.h");
    @cInclude("unistd.h");
    @cInclude("fcntl.h");
    @cInclude("sys/stat.h");
    @cInclude("sys/file.h");
    @cInclude("sys/utsname.h");
    @cInclude("glob.h");
    @cInclude("fnmatch.h");
    @cInclude("stdio.h");
    @cInclude("stdlib.h");
    @cInclude("time.h");
});
