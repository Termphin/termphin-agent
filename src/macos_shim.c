#include <libproc.h>
#include <sys/proc_info.h>
#include <string.h>

int termphin_agent_shell_cwd(pid_t pid, char *out, int out_len) {
    struct proc_vnodepathinfo info;
    int n = proc_pidinfo(pid, PROC_PIDVNODEPATHINFO, 0, &info, sizeof(info));
    if (n <= 0) {
        return -1;
    }
    size_t len = strnlen(info.pvi_cdir.vip_path, sizeof(info.pvi_cdir.vip_path));
    if ((int)len >= out_len) {
        len = out_len - 1;
    }
    memcpy(out, info.pvi_cdir.vip_path, len);
    out[len] = '\0';
    return (int)len;
}
