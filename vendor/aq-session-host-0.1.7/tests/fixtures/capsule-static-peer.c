#define _GNU_SOURCE
#include <arpa/inet.h>
#include <dirent.h>
#include <errno.h>
#include <fcntl.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <unistd.h>
#define CHECK(x) do { if (!(x)) _exit(101); } while (0)
static void denied(const char *p) { int fd = open(p, O_RDONLY | O_CLOEXEC); CHECK(fd < 0 && (errno == ENOENT || errno == EACCES || errno == EPERM)); }
static void exact(const char *p, const char *value) { char b[128]; int fd = open(p, O_RDONLY | O_CLOEXEC); CHECK(fd >= 0); ssize_t n = read(fd, b, sizeof(b)); CHECK(n == (ssize_t)strlen(value) && !memcmp(b,value,n)); close(fd); }
int main(int argc, char **argv) {
 CHECK((argc == 7 || argc == 10) && getpid() > 1 && fcntl(3,F_GETFD) < 0 && errno == EBADF);
 CHECK(getenv("HOME") == NULL && getenv("AQ_SESSION_DIRECTORY") == NULL && getenv("TMPDIR") == NULL);
 const char *expected="capsule exact stdin bytes\n";char input[128];ssize_t n=read(0,input,sizeof(input));CHECK(n==(ssize_t)strlen(expected) && !memcmp(input,expected,n));CHECK(read(0,input,sizeof(input))==0);
 if(argc==7)exact("/auth/auth.json","derived-local-canary\n");else{
  char auth[256]={0},cfg[4096]={0};const char *prefix="{\"OPENAI_API_KEY\":\"aq-responses-";int fd=open("/auth/auth.json",O_RDONLY);CHECK(fd>=0);ssize_t n=read(fd,auth,sizeof(auth)-1);close(fd);CHECK(n>0 && !strncmp(auth,prefix,strlen(prefix)));
  fd=open("/auth/candidate-config.json",O_RDONLY);CHECK(fd>=0);n=read(fd,cfg,sizeof(cfg)-1);close(fd);CHECK(n>0 && strstr(cfg,getenv("AQ_CAPSULE_ENDPOINT")));
  CHECK(!strncmp(argv[7],"--candidate-origin=",19) && !strcmp(argv[7]+19,getenv("AQ_CAPSULE_ENDPOINT")) && !strncmp(argv[8],"--candidate-preparation=",24) && !strncmp(argv[9],"--candidate-deadline=",21));
 }
 exact("/allowed/0/item","allowed-canary\n"); exact("/git-metadata/item","git-canary\n");
 CHECK(open("/auth/auth.json",O_WRONLY | O_CLOEXEC) < 0 && errno == EROFS);
 denied(argv[4]); denied("/.old-root"); denied("/home/operator/.profile"); denied("/var/lib/aq-node/material");
 int sourcefd=atoi(argv[3]); char path[4096];
 snprintf(path,sizeof(path),"/proc/%s/root%s",argv[1],argv[4]);denied(path);
 for(char *tid=strtok(argv[2],",");tid;tid=strtok(NULL,",")){
  snprintf(path,sizeof(path),"/proc/%s/fd/%d",tid,sourcefd);denied(path);
  snprintf(path,sizeof(path),"/proc/%s/task/%s/fd/%d",argv[1],tid,sourcefd);denied(path);
 }
 snprintf(path,sizeof(path),"/proc/1/root%s",argv[4]);denied(path);
 unsigned long long dev=strtoull(argv[5],NULL,10),ino=strtoull(argv[6],NULL,10);
 DIR *dir=opendir("/proc/self/fd");CHECK(dir!=NULL);struct dirent *entry;
 while((entry=readdir(dir))){if(entry->d_name[0]=='.')continue;struct stat st;snprintf(path,sizeof(path),"/proc/self/fd/%s",entry->d_name);CHECK(stat(path,&st)==0 && ((unsigned long long)st.st_dev!=dev || (unsigned long long)st.st_ino!=ino));}closedir(dir);
 CHECK(socket(AF_UNIX,SOCK_STREAM,0)<0 && errno==EPERM);int pair[2];CHECK(socketpair(AF_UNIX,SOCK_DGRAM,0,pair)<0 && errno==EPERM);
 CHECK(socketpair(AF_UNIX,SOCK_SEQPACKET,0,pair)==0);close(pair[0]);close(pair[1]);
 CHECK(syscall(SYS_io_uring_setup,1,NULL)<0 && errno==EPERM);
 pid_t child=fork();CHECK(child>=0);if(!child){CHECK(socket(AF_UNIX,SOCK_STREAM,0)<0 && errno==EPERM);for(;;)pause();}
 int report=open("/workspace/checks-passed",O_CREAT|O_EXCL|O_WRONLY,0600);CHECK(report>=0 && write(report,"static mechanism only\n",22)==22);close(report);
 const char *origin=getenv("AQ_CAPSULE_ENDPOINT");CHECK(origin!=NULL);int port=0;CHECK(sscanf(origin,"http://127.0.0.1:%d",&port)==1 && port>0);
 int fd=socket(AF_INET,SOCK_STREAM|SOCK_CLOEXEC,0);CHECK(fd>=0);struct sockaddr_in a={.sin_family=AF_INET,.sin_port=htons(port),.sin_addr.s_addr=htonl(INADDR_LOOPBACK)};CHECK(connect(fd,(void*)&a,sizeof(a))==0 && write(fd,"static-proxy-ready",18)==18);
 char reply;CHECK(read(fd,&reply,1)==1 && reply=='x');close(fd);for(;;)pause();
}
