#define _GNU_SOURCE
#include <unistd.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <fcntl.h>
#include <sys/stat.h>
#include <sys/statvfs.h>
#include <sys/wait.h>

/* Explicit static physical fixture. No agent, provider, auth, shell or network. */
int main(int argc,char **argv) {
 char path[4096];ssize_t n=readlink("/proc/self/exe",path,sizeof(path)-1);if(n<0)return 60;path[n]=0;
 char *canonical=realpath(path,NULL);if(!canonical||strcmp(path,canonical)||strcmp(path,"/opt/aq-qualification/codex"))return 61;free(canonical);
 struct stat image;struct statvfs fs;int self=open(path,O_RDONLY|O_NOFOLLOW);
 if(self<0||fstat(self,&image)||fstatvfs(self,&fs)||!(fs.f_flag&ST_RDONLY))return 62;
 close(self);
 if(argc==4&&!strcmp(argv[1],"--helper")) {
  if((unsigned long long)image.st_dev!=strtoull(argv[2],NULL,10)||(unsigned long long)image.st_ino!=strtoull(argv[3],NULL,10))return 63;
  puts("NAMED-HELPER:same-inode-canonical");fflush(stdout);return 0;
 }
 if(argc!=1)return 64;
 const char payload[]="named readonly sealed stdin\n";char input[128];size_t got=0;ssize_t count;
 while((count=read(0,input+got,sizeof(input)-got))>0){got+=count;if(got==sizeof(input))return 65;}
 int seals=fcntl(0,F_GET_SEALS),required=F_SEAL_WRITE|F_SEAL_GROW|F_SEAL_SHRINK|F_SEAL_SEAL;
 if(count<0||got!=sizeof(payload)-1||memcmp(input,payload,got)||(seals&required)!=required)return 66;
 puts("NAMED-STDIN:exact-sealed");printf("NAMED-EXE:%s\n",path);fflush(stdout);
 const char *home=getenv("CODEX_HOME");if(!home||strncmp(home,"/var/lib/aq-qualification/",strlen("/var/lib/aq-qualification/")))return 67;
 if(mkdir(home,0700))return 68;
 char helper[4096],device[40],inode[40];if(snprintf(helper,sizeof(helper),"%s/peer-helper",home)<0)return 69;
 if(symlink(path,helper))return 70;
 snprintf(device,sizeof(device),"%llu",(unsigned long long)image.st_dev);snprintf(inode,sizeof(inode),"%llu",(unsigned long long)image.st_ino);
 pid_t child=fork();if(child<0)return 71;if(child==0){execl(helper,helper,"--helper",device,inode,NULL);_exit(72);}
 int status;if(waitpid(child,&status,0)!=child||!WIFEXITED(status)||WEXITSTATUS(status)!=0)return 73;
 puts("NAMED-READY:physical-only");fflush(stdout);
 for(;;)pause();
}
