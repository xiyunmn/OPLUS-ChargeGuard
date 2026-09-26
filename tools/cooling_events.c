// Cooling kernel event transport. Never reads or writes control nodes.
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <linux/genetlink.h>
#include <linux/netlink.h>
#include <linux/perf_event.h>
#include <linux/thermal.h>
#include <poll.h>
#include <signal.h>
#include <stdarg.h>
#include <stdbool.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/prctl.h>
#include <sys/socket.h>
#include <sys/syscall.h>
#include <time.h>
#include <unistd.h>

#define MAX_PERF 64
static volatile sig_atomic_t stopping;
static unsigned long seq, lost_records;
static bool parent_closed, output_failed;
static int genfd=-1, uefd=-1, family=-1;
static uint64_t retry_ns;
static struct { int fd, cpu; void *map; size_t bytes; char source[96]; } perfs[MAX_PERF];
static int nperf;
static bool parent_open(void);
static uint64_t clock_ns(clockid_t id) { struct timespec t; clock_gettime(id,&t); return (uint64_t)t.tv_sec*1000000000+t.tv_nsec; }
static void stopped(int sig) { (void)sig; stopping=1; }
static void out(const char *s) {
    if(output_failed)return;
    if(fputs(s,stdout)==EOF) {output_failed=true;stopping=1;return;}
}
static void fmt(const char *s,...) { char b[4096]; va_list a; va_start(a,s); vsnprintf(b,sizeof(b),s,a); va_end(a); out(b); }
static void quote(const char *s) { out("\""); for(;*s;s++) { unsigned char c=*s; if(c=='"'||c=='\\') fmt("\\%c",c); else if(c<32||c>=127) fmt("\\u%04x",c); else { char b[2]={(char)c,0}; out(b); } } out("\""); }
static void begin(const char *kind) {
    fmt("{\"schema\":1,\"sequence\":%lu,\"monotonic_ns\":%llu,\"boottime_ns\":%llu,\"event\":",++seq,(unsigned long long)clock_ns(CLOCK_MONOTONIC),(unsigned long long)clock_ns(CLOCK_BOOTTIME)); quote(kind);
}
static void finish(void) { out("}\n"); }
static void text_field(const char *key,const char *value) { out(",\""); out(key); out("\":"); quote(value); }
static void error_record(const char *source,int error) { begin("source_unavailable"); text_field("source",source); fmt(",\"errno\":%d",error); finish(); }
static ssize_t read_path(const char *p,char *b,size_t cap) {
    int fd=open(p,O_RDONLY|O_CLOEXEC|O_NONBLOCK); if(fd<0)return -1;
    ssize_t n=read(fd,b,cap-1); int e=errno; close(fd); errno=e;
    if(n>=0) { while(n>0&&(b[n-1]=='\n'||b[n-1]=='\r'))n--; b[n]=0; } return n;
}
static void rebuilding(const char *source) {
    begin("listeners_rebuild");text_field("source",source);finish();
}
static int nl_socket(int protocol) {
    int fd=socket(AF_NETLINK,SOCK_RAW|SOCK_CLOEXEC|SOCK_NONBLOCK,protocol);
    if(fd<0)return -1; struct sockaddr_nl a={.nl_family=AF_NETLINK,.nl_groups=protocol==NETLINK_KOBJECT_UEVENT?1:0};
    if(bind(fd,(void*)&a,sizeof(a))<0) { int e=errno; close(fd); errno=e; return -1; } return fd;
}
static struct nlattr *attr_next(struct nlattr *a) { return (void*)((char*)a+NLA_ALIGN(a->nla_len)); }
static bool attr_ok(struct nlattr *a,char *end) { return (char*)a+NLA_HDRLEN<=end&&a->nla_len>=NLA_HDRLEN&&(char*)a+a->nla_len<=end; }
static void *attr_data(struct nlattr *a) { return (char*)a+NLA_HDRLEN; }
static void open_generic(void) {
    rebuilding("thermal_netlink");
    if(genfd>=0)close(genfd); genfd=nl_socket(NETLINK_GENERIC); family=-1;
    if(genfd<0) { error_record("thermal_netlink",errno); return; }
    char req[128]={0},buf[8192]; struct nlmsghdr *h=(void*)req; struct genlmsghdr *g=(void*)(req+NLMSG_HDRLEN);
    struct nlattr *a=(void*)(req+NLMSG_HDRLEN+GENL_HDRLEN);
    a->nla_type=CTRL_ATTR_FAMILY_NAME; a->nla_len=NLA_HDRLEN+sizeof(THERMAL_GENL_FAMILY_NAME); memcpy(attr_data(a),THERMAL_GENL_FAMILY_NAME,sizeof(THERMAL_GENL_FAMILY_NAME));
    h->nlmsg_len=NLMSG_HDRLEN+GENL_HDRLEN+NLA_ALIGN(a->nla_len);h->nlmsg_type=GENL_ID_CTRL;h->nlmsg_flags=NLM_F_REQUEST;h->nlmsg_seq=1;g->cmd=CTRL_CMD_GETFAMILY;g->version=1;
    struct sockaddr_nl dest={.nl_family=AF_NETLINK};
    if(sendto(genfd,req,h->nlmsg_len,0,(void*)&dest,sizeof(dest))<0)goto failed;
    struct pollfd p[2]={{genfd,POLLIN,0},{STDIN_FILENO,POLLIN,0}};
    if(poll(p,2,1000)<=0) {errno=ETIMEDOUT;goto failed;}
    if(!parent_open()||stopping) {close(genfd);genfd=-1;return;}
    if(!(p[0].revents&POLLIN)) {errno=EIO;goto failed;}
    ssize_t n=recv(genfd,buf,sizeof(buf),0); int group=-1;
    for(h=(void*)buf;n>0&&NLMSG_OK(h,n);h=NLMSG_NEXT(h,n)) {
        if(h->nlmsg_type==NLMSG_ERROR||h->nlmsg_len<NLMSG_HDRLEN+GENL_HDRLEN)continue;
        char *end=(char*)h+h->nlmsg_len;
        for(a=(void*)((char*)NLMSG_DATA(h)+GENL_HDRLEN);attr_ok(a,end);a=attr_next(a)) {
            int t=a->nla_type&NLA_TYPE_MASK;
            if(t==CTRL_ATTR_FAMILY_ID&&a->nla_len>=NLA_HDRLEN+2) { uint16_t id;memcpy(&id,attr_data(a),2);family=id; }
            if(t!=CTRL_ATTR_MCAST_GROUPS)continue;
            char *ge=(char*)a+a->nla_len;
            for(struct nlattr *b=attr_data(a);attr_ok(b,ge);b=attr_next(b)) {
                char name[128]={0};int id=-1;char *be=(char*)b+b->nla_len;
                for(struct nlattr *c=attr_data(b);attr_ok(c,be);c=attr_next(c)) {
                    int ct=c->nla_type&NLA_TYPE_MASK;
                    if(ct==CTRL_ATTR_MCAST_GRP_NAME)snprintf(name,sizeof(name),"%.*s",(int)c->nla_len-NLA_HDRLEN,(char*)attr_data(c));
                    if(ct==CTRL_ATTR_MCAST_GRP_ID&&c->nla_len>=NLA_HDRLEN+4)memcpy(&id,attr_data(c),4);
                }
                if(!strcmp(name,THERMAL_GENL_EVENT_GROUP_NAME))group=id;
            }
        }
    }
    if(family<0||group<0||setsockopt(genfd,SOL_NETLINK,NETLINK_ADD_MEMBERSHIP,&group,sizeof(group))<0)goto failed;
    begin("source_ready");text_field("source","thermal_netlink");fmt(",\"family\":%d,\"group\":%d",family,group);finish();return;
failed:
    error_record("thermal_netlink",errno);close(genfd);genfd=-1;
}
static void drain_netlink(int fd,bool uevent) {
    char b[65536]; struct sockaddr_nl from; socklen_t alen=sizeof(from);
    for(int burst=0;burst<16;burst++) {
        struct iovec iov={b,sizeof(b)};struct msghdr msg={.msg_name=&from,.msg_namelen=alen,.msg_iov=&iov,.msg_iovlen=1};
        ssize_t n=recvmsg(fd,&msg,0);
        if(n<0) {
            if(errno!=EAGAIN&&errno!=EINTR) {
                lost_records++;error_record(uevent?"uevent":"thermal_netlink",errno);
                close(fd);if(uevent)uefd=-1;else genfd=-1;
            }break;
        }
        if(!n||msg.msg_flags&MSG_TRUNC) {
            lost_records++;begin("notification_loss");text_field("source",uevent?"uevent":"thermal_netlink");finish();
            close(fd);if(uevent)uefd=-1;else genfd=-1;break;
        }
        if(from.nl_pid)continue;
        if(uevent) {
            bool relevant=false;char printable[4096];size_t outn=0;
            for(size_t at=0;at<(size_t)n;) {
                size_t len=strnlen(b+at,n-at);if(at+len>=(size_t)n)break;
                if(!strcmp(b+at,"SUBSYSTEM=thermal")||!strcmp(b+at,"SUBSYSTEM=module")||!strcmp(b+at,"SUBSYSTEM=platform")||!strcmp(b+at,"SUBSYSTEM=cpu"))relevant=true;
                if(outn+len+2<sizeof(printable)) {memcpy(printable+outn,b+at,len);outn+=len;printable[outn++]=' ';}at+=len+1;
            }
            if(relevant) { printable[outn]=0;begin("topology_event");text_field("fields",printable);finish();retry_ns=0; }
            continue;
        }
        for(struct nlmsghdr *h=(void*)b;NLMSG_OK(h,n);h=NLMSG_NEXT(h,n)) {
            if(h->nlmsg_type==NLMSG_OVERRUN||h->nlmsg_type==NLMSG_ERROR) {
                lost_records++;begin("notification_loss");text_field("source","thermal_netlink");finish();close(fd);genfd=-1;return;
            }
            if(h->nlmsg_type!=family||h->nlmsg_len<NLMSG_HDRLEN+GENL_HDRLEN)continue;
            struct genlmsghdr *g=NLMSG_DATA(h);int id=-1;uint64_t state=0;bool has=false;char *end=(char*)h+h->nlmsg_len;
            for(struct nlattr *a=(void*)((char*)g+GENL_HDRLEN);attr_ok(a,end);a=attr_next(a)) {
                int t=a->nla_type&NLA_TYPE_MASK;
                if(t==THERMAL_GENL_ATTR_CDEV_ID&&a->nla_len>=NLA_HDRLEN+4)memcpy(&id,attr_data(a),4);
                if(t==THERMAL_GENL_ATTR_CDEV_CUR_STATE&&a->nla_len>=NLA_HDRLEN+4) {memcpy(&state,attr_data(a),a->nla_len>=NLA_HDRLEN+8?8:4);has=true;}
            }
            begin("thermal_netlink");fmt(",\"command\":%u,\"cdev_id\":%d,\"state\":",g->cmd,id);if(has)fmt("%llu",(unsigned long long)state);else out("null");finish();
        }
    }
}
static void close_perfs(void) {
    for(int i=0;i<nperf;i++) {munmap(perfs[i].map,perfs[i].bytes);close(perfs[i].fd);}nperf=0;
}
static void close_perf(int index) {
    munmap(perfs[index].map,perfs[index].bytes);close(perfs[index].fd);perfs[index]=perfs[--nperf];
}
static bool online(int cpu) { char p[128],b[64];snprintf(p,sizeof(p),"/sys/devices/system/cpu/cpu%d/online",cpu);return cpu==0||(read_path(p,b,sizeof(b))>0&&atoi(b)==1); }
static bool has_perf(const char *source,int cpu) {
    for(int i=0;i<nperf;i++)if(perfs[i].cpu==cpu&&!strcmp(perfs[i].source,source))return true;
    return false;
}
static void perf_source(const char *source) {
    if(stopping||!parent_open())return;
    long cpus=sysconf(_SC_NPROCESSORS_CONF),page=sysconf(_SC_PAGESIZE);if(cpus>64)cpus=64;
    bool needed=false;
    for(int cpu=0;cpu<cpus;cpu++)if(online(cpu)&&!has_perf(source,cpu))needed=true;
    if(!needed)return;
    rebuilding(source);
    char path[256],b[8192]; struct perf_event_attr a={0}; a.size=sizeof(a);a.sample_period=1;
    a.sample_type=PERF_SAMPLE_TID|PERF_SAMPLE_TIME|PERF_SAMPLE_CPU|PERF_SAMPLE_RAW;
    a.wakeup_events=1;a.use_clockid=1;a.clockid=CLOCK_MONOTONIC;
    snprintf(path,sizeof(path),"/sys/kernel/tracing/events/%s/id",source);
    if(read_path(path,b,sizeof(b))<0) {error_record(source,errno);return;}
    a.type=PERF_TYPE_TRACEPOINT;a.config=strtoull(b,NULL,10);
    snprintf(path,sizeof(path),"/sys/kernel/tracing/events/%s/format",source);
    if(read_path(path,b,sizeof(b))>=0) {begin("trace_format");text_field("source",source);text_field("format",b);finish();}
    for(int cpu=0;cpu<cpus&&nperf<MAX_PERF;cpu++) {
        if(stopping||!parent_open())return;
        if(!online(cpu)||has_perf(source,cpu))continue;
        int fd=syscall(__NR_perf_event_open,&a,-1,cpu,-1,PERF_FLAG_FD_CLOEXEC);
        if(fd<0) {begin("perf_unavailable");text_field("source",source);fmt(",\"cpu\":%d,\"errno\":%d",cpu,errno);finish();continue;}
        size_t bytes=(size_t)page*9;void *map=mmap(NULL,bytes,PROT_READ|PROT_WRITE,MAP_SHARED,fd,0);
        if(map==MAP_FAILED) {error_record(source,errno);close(fd);continue;}
        perfs[nperf].fd=fd;perfs[nperf].map=map;perfs[nperf].bytes=bytes;perfs[nperf].cpu=cpu;
        snprintf(perfs[nperf].source,sizeof(perfs[nperf].source),"%s",source);nperf++;
        begin("perf_ready");text_field("source",source);fmt(",\"cpu\":%d",cpu);finish();
    }
}
static void open_perfs(void) {
    for(int i=nperf-1;i>=0;i--)if(!online(perfs[i].cpu))close_perf(i);
    perf_source("thermal/cdev_update");
}
static void ring_copy(char *dest,const char *ring,size_t size,uint64_t at,size_t n) {
    size_t pos=at%size,first=n<size-pos?n:size-pos;memcpy(dest,ring+pos,first);memcpy(dest+first,ring,n-first);
}
static void drain_perf(int i) {
    struct perf_event_mmap_page *p=perfs[i].map;
    uint64_t head=__atomic_load_n(&p->data_head,__ATOMIC_ACQUIRE),tail=p->data_tail;
    size_t size=p->data_size,offset=p->data_offset;
    if(!size||offset>perfs[i].bytes||size>perfs[i].bytes-offset) {lost_records++;begin("notification_loss");text_field("source",perfs[i].source);finish();return;}
    const char *ring=(char*)p+offset;
    if(head-tail>size) {lost_records++;tail=head;begin("notification_loss");text_field("source",perfs[i].source);finish();}
    for(int burst=0;tail<head&&burst<512;burst++) {
        struct perf_event_header hdr;
        if(head-tail<sizeof(hdr))break;
        ring_copy((char*)&hdr,ring,size,tail,sizeof(hdr));
        if(hdr.size<sizeof(hdr)||hdr.size>size||hdr.size>head-tail) {lost_records++;tail=head;begin("notification_loss");text_field("source",perfs[i].source);finish();break;}
        unsigned char data[8192];
        if(hdr.size>sizeof(data)) {lost_records++;tail+=hdr.size;begin("notification_loss");text_field("source",perfs[i].source);finish();continue;}
        ring_copy((char*)data,ring,size,tail,hdr.size);tail+=hdr.size;
        if(hdr.type==PERF_RECORD_LOST||hdr.type==PERF_RECORD_LOST_SAMPLES) {lost_records++;begin("notification_loss");text_field("source",perfs[i].source);finish();continue;}
        if(hdr.type!=PERF_RECORD_SAMPLE||hdr.size<32)continue;
        uint32_t pid,tid,cpu;uint64_t ts;memcpy(&pid,data+8,4);memcpy(&tid,data+12,4);memcpy(&ts,data+16,8);memcpy(&cpu,data+24,4);
        begin("kernel_event");text_field("source",perfs[i].source);fmt(",\"event_monotonic_ns\":%llu,\"context_pid\":%u,\"context_tid\":%u,\"cpu\":%u",(unsigned long long)ts,pid,tid,cpu);
        out(",\"raw_hex\":\"");
        if(hdr.size>=36) {uint32_t n;memcpy(&n,data+32,4);if(n<=hdr.size-36)for(uint32_t j=0;j<n;j++)fmt("%02x",data[36+j]);else lost_records++;}
        out("\"");finish();
    }
    __atomic_store_n(&p->data_tail,tail,__ATOMIC_RELEASE);
}
static bool parent_open(void) {
    struct pollfd fd={STDIN_FILENO,POLLIN,0};
    if(poll(&fd,1,0)<0)return errno==EINTR;
    if(fd.revents&(POLLHUP|POLLERR|POLLNVAL)) {parent_closed=true;return false;}
    if(fd.revents&POLLIN) {
        char discard[256];ssize_t n=read(STDIN_FILENO,discard,sizeof(discard));
        if(n==0||(n<0&&errno!=EAGAIN&&errno!=EINTR)) {parent_closed=true;return false;}
    }
    return true;
}
// Long-lived notification transport. It never discovers/reads control nodes,
// samples temperatures or touches global tracefs switches.
// Periodic work checks transport/CPU coverage only; node decisions stay in cg.
int main(int argc,char **argv) {
    (void)argv;
    if(argc!=1) {fputs("usage: cg-cooling-events\n",stderr);return 2;}
    if(geteuid()!=0)return 2;
    int flags=fcntl(STDIN_FILENO,F_GETFL),output_flags=fcntl(STDOUT_FILENO,F_GETFL);
    if(flags<0||output_flags<0||fcntl(STDIN_FILENO,F_SETFL,flags|O_NONBLOCK)<0||fcntl(STDOUT_FILENO,F_SETFL,output_flags|O_NONBLOCK)<0)return 2;
    pid_t parent=getppid();signal(SIGINT,stopped);signal(SIGTERM,stopped);signal(SIGPIPE,stopped);prctl(PR_SET_PDEATHSIG,SIGTERM);
    if(getppid()!=parent)stopping=1;
    setvbuf(stdout,NULL,_IOLBF,0);
    begin("cooling_listener_start");out(",\"control_reads\":false,\"writes_controls\":false,\"parent_pipe\":true");finish();
    while(!stopping&&parent_open()) {
        uint64_t now=clock_ns(CLOCK_BOOTTIME);
        if(now>=retry_ns) {
            if(genfd<0)open_generic();open_perfs();
            if(uefd<0) {uefd=nl_socket(NETLINK_KOBJECT_UEVENT);if(uefd<0)error_record("uevent",errno);}
            long cpus=sysconf(_SC_NPROCESSORS_CONF);bool complete=cpus>0&&cpus<=64;
            for(int cpu=0;cpu<cpus&&cpu<64;cpu++)if(online(cpu)&&!has_perf("thermal/cdev_update",cpu))complete=false;
            begin("cooling_health");fmt(",\"netlink_ready\":%s,\"uevent_ready\":%s,\"perf_complete\":%s,\"lost_records\":%lu,\"online_cpus\":[",genfd>=0?"true":"false",uefd>=0?"true":"false",complete?"true":"false",lost_records);
            bool comma=false;for(int cpu=0;cpu<cpus&&cpu<64;cpu++)if(online(cpu)) {fmt("%s%d",comma?",":"",cpu);comma=true;}out("]");finish();
            retry_ns=clock_ns(CLOCK_BOOTTIME)+10000000000ull;
        }
        if(!parent_open()||stopping)break;
        struct pollfd fds[MAX_PERF+3]={{genfd,POLLIN,0},{uefd,POLLIN,0},{STDIN_FILENO,POLLIN,0}};
        for(int i=0;i<nperf;i++)fds[i+3]=(struct pollfd){perfs[i].fd,POLLIN,0};
        now=clock_ns(CLOCK_BOOTTIME);int timeout=retry_ns>now?(int)((retry_ns-now+999999)/1000000):0;
        int r=poll(fds,nperf+3,timeout);
        if(r<0) {if(errno==EINTR)continue;error_record("poll",errno);break;}
        if(!parent_open())break;
        if(fds[0].revents&POLLIN)drain_netlink(genfd,false);
        if(fds[1].revents&POLLIN)drain_netlink(uefd,true);
        for(int i=0;i<2;i++)if(fds[i].revents&(POLLERR|POLLHUP|POLLNVAL)) {
            lost_records++;begin("notification_loss");text_field("source",i==0?"thermal_netlink":"uevent");finish();
            if(i==0&&genfd>=0) {close(genfd);genfd=-1;}else if(i==1&&uefd>=0) {close(uefd);uefd=-1;}retry_ns=0;
        }
        for(int i=nperf-1;i>=0;i--) {
            if(fds[i+3].revents&(POLLERR|POLLHUP|POLLNVAL)) {
                lost_records++;begin("notification_loss");text_field("source",perfs[i].source);finish();close_perf(i);retry_ns=0;
            } else if(fds[i+3].revents&POLLIN)drain_perf(i);
        }
    }
    begin("cooling_listener_end");text_field("reason",parent_closed?"parent_pipe_closed":"stopped");finish();
    close_perfs();if(genfd>=0)close(genfd);if(uefd>=0)close(uefd);return output_failed?4:0;
}