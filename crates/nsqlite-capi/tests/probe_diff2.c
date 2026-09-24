/* Differential probe: runs the same cases against whatever sqlite3.h resolves
** to at link time. Build once against the shim, once against the real library,
** and diff the output.
*/
#include <stdio.h>
#include <string.h>
#include "sqlite3.h"

static void P(const char*sql){
  sqlite3*db;sqlite3_stmt*st;int rc;int i;int n;
  sqlite3_open(":memory:",&db);
  st=0; rc=sqlite3_prepare_v2(db,sql,-1,&st,0);
  if(rc!=0){printf("%-34s PREPARE_ERR=%d\n",sql,rc);sqlite3_close(db);return;}
  n=sqlite3_bind_parameter_count(st);
  printf("%-34s count=%d names=[",sql,n);
  for(i=1;i<=n;i++){const char*s=sqlite3_bind_parameter_name(st,i);
    printf("%s%s",i>1?",":"",s?s:"(null)");}
  printf("] idx(:x)=%d idx(@x)=%d idx($x)=%d idx(:1)=%d idx(?1)=%d\n",
    sqlite3_bind_parameter_index(st,":x"),sqlite3_bind_parameter_index(st,"@x"),
    sqlite3_bind_parameter_index(st,"$x"),sqlite3_bind_parameter_index(st,":1"),
    sqlite3_bind_parameter_index(st,"?1"));
  sqlite3_finalize(st);sqlite3_close(db);
}

int main(void){
  setbuf(stdout,NULL);
  printf("### PARAMETERS\n");
  P("SELECT @x, @x, :x, $x");
  P("SELECT :x, @x");
  P("SELECT :x, :x, ?5, ?5, ?9");
  P("SELECT :1");
  P("SELECT ?1");
  P("SELECT :abc");
  P("SELECT :$, :0, :$x");
  P("SELECT :a, :a, ?5, ?5, ?9");

  printf("### RESET\n");
  { sqlite3*db;sqlite3_stmt*st;int rc;
    sqlite3_open(":memory:",&db);
    sqlite3_exec(db,"CREATE TABLE t(a)",0,0,0);
    sqlite3_exec(db,"INSERT INTO t VALUES(1),(2),(3)",0,0,0);
    sqlite3_prepare_v2(db,"SELECT a FROM t WHERE a=999",-1,&st,0);
    rc=sqlite3_step(st); printf("step-empty=%d ",rc);
    rc=sqlite3_reset(st); printf("reset=%d\n",rc);
    sqlite3_finalize(st);
    sqlite3_prepare_v2(db,"SELECT a FROM t",-1,&st,0);
    rc=sqlite3_step(st); printf("step-row=%d ",rc);
    rc=sqlite3_reset(st); printf("reset=%d\n",rc);
    sqlite3_finalize(st);
    sqlite3_close(db); }

  printf("### ERRSTR\n");
  { int codes[]={0,1,2,3,4,5,6,7,8,9,10,11,12,13,14,15,16,17,18,19,20,21,22,23,24,25,26,27,28,100,101,787,1555,9999};
    for(unsigned k=0;k<sizeof(codes)/sizeof(*codes);k++)
      printf("errstr(%d)=%s\n",codes[k],sqlite3_errstr(codes[k])); }

  printf("### CHANGES\n");
  { sqlite3*db;sqlite3_stmt*st;
    sqlite3_open(":memory:",&db);
    sqlite3_exec(db,"CREATE TABLE t(a)",0,0,0);
    sqlite3_exec(db,"INSERT INTO t VALUES(1),(2),(3),(4),(5)",0,0,0);
    printf("after-insert changes=%d total=%d\n",sqlite3_changes(db),sqlite3_total_changes(db));
    sqlite3_prepare_v2(db,"SELECT a FROM t",-1,&st,0);
    printf("after-prepare changes=%d total=%d\n",sqlite3_changes(db),sqlite3_total_changes(db));
    sqlite3_step(st);
    printf("after-step    changes=%d total=%d\n",sqlite3_changes(db),sqlite3_total_changes(db));
    sqlite3_finalize(st);
    sqlite3_exec(db,"UPDATE t SET a=a WHERE a=99",0,0,0);
    printf("after-0row-upd changes=%d total=%d\n",sqlite3_changes(db),sqlite3_total_changes(db));
    sqlite3_close(db); }

  printf("### COLINT64 / COLDOUBLE on TEXT\n");
  { sqlite3*db;sqlite3_stmt*st;
    sqlite3_open(":memory:",&db);
    const char*vals[]={"1e3","1.9e2","inf","nan","1e400","42abc",".5",
                       "9223372036854775808","9007199254740993","1.5e2xyz","3e"};
    for(unsigned k=0;k<sizeof(vals)/sizeof(*vals);k++){
      char sql[64]; snprintf(sql,sizeof sql,"SELECT '%s'",vals[k]);
      sqlite3_prepare_v2(db,sql,-1,&st,0);sqlite3_step(st);
      printf("%-22s i64=%lld dbl=%.17g\n",vals[k],
        (long long)sqlite3_column_int64(st,0),sqlite3_column_double(st,0));
      sqlite3_finalize(st); }
    sqlite3_close(db); }

  printf("### CLOSE\n");
  { sqlite3*db;sqlite3_stmt*a,*b;
    sqlite3_open(":memory:",&db);
    sqlite3_prepare_v2(db,"SELECT 1",-1,&a,0);
    sqlite3_prepare_v2(db,"SELECT 2",-1,&b,0);
    printf("close2=%d\n",sqlite3_close(db));
    sqlite3_finalize(a);
    printf("close1=%d\n",sqlite3_close(db));
    sqlite3_finalize(b);
    printf("close0=%d\n",sqlite3_close(db)); }

  printf("### PREPARE ERRORS\n");
  { sqlite3*db;sqlite3_stmt*st;int rc;
    sqlite3_open(":memory:",&db);
    st=0; rc=sqlite3_prepare_v2(db,"SELECT * FROM no_such_table_xyz",-1,&st,0);
    printf("badtable rc=%d stmtnull=%d\n",rc,st==0);
    st=0; rc=sqlite3_prepare_v2(db,"SELECT nosuchcol FROM (SELECT 1)",-1,&st,0);
    printf("badcol   rc=%d stmtnull=%d\n",rc,st==0);
    sqlite3_close(db); }

  printf("### COLUMN_TYPE AFTER SIBLING ACCESSOR\n");
  { sqlite3*db;sqlite3_stmt*st;int t0,t1;
    sqlite3_open(":memory:",&db);
    sqlite3_exec(db,"CREATE TABLE b(x); INSERT INTO b VALUES(x'0102ff'),(x'03');",0,0,0);
    const char*qs[]={"SELECT 42","SELECT 1.5","SELECT NULL","SELECT 'abc'",
                     "SELECT x'0102ff'","SELECT x FROM b","SELECT x FROM b"};
    for(int k=0;k<6;k++){
      sqlite3_prepare_v2(db,qs[k],-1,&st,0);sqlite3_step(st);
      t0=sqlite3_column_type(st,0);
      sqlite3_column_text(st,0);
      t1=sqlite3_column_type(st,0);
      printf("%-42s %d->%d",qs[k],t0,t1);
      if(k==5){
        sqlite3_step(st);
        printf(" nextrow=%d",sqlite3_column_type(st,0));
        sqlite3_reset(st);sqlite3_step(st);
        printf(" afterreset=%d",sqlite3_column_type(st,0));
      }
      printf("\n");
      sqlite3_finalize(st); }
    sqlite3_close(db); }

  printf("### READONLY\n");
  { sqlite3*db;sqlite3_stmt*st;int rc;char*err=0;
    remove("probe_ro.db");
    sqlite3_open("probe_ro.db",&db);
    sqlite3_exec(db,"CREATE TABLE t(a); INSERT INTO t VALUES(1);",0,0,0);
    sqlite3_close(db);
    rc=sqlite3_open_v2("probe_ro.db",&db,SQLITE_OPEN_READONLY,0);
    printf("open-ro rc=%d\n",rc);
    if(rc==0){
      printf("read  rc=%d\n",sqlite3_exec(db,"SELECT a FROM t",0,0,0));
      err=0; rc=sqlite3_exec(db,"INSERT INTO t VALUES(2)",0,0,&err);
      printf("write rc=%d\n",rc); sqlite3_free(err); err=0;
      err=0; rc=sqlite3_exec(db,"CREATE TABLE u(b)",0,0,&err);
      printf("ddl   rc=%d\n",rc); sqlite3_free(err); err=0;
      err=0; rc=sqlite3_exec(db,"BEGIN",0,0,&err);
      printf("begin rc=%d\n",rc); sqlite3_free(err); err=0;
      err=0; rc=sqlite3_exec(db,"COMMIT",0,0,&err);
      printf("commit rc=%d\n",rc); sqlite3_free(err);
      sqlite3_prepare_v2(db,"INSERT INTO t VALUES(9)",-1,&st,0);
      printf("prep-write rc=%d\n",st?0:-1);
      if(st){
        rc=sqlite3_step(st); printf("step-write rc=%d\n",rc);
        rc=sqlite3_reset(st);  printf("reset-write rc=%d\n",rc);
        rc=sqlite3_finalize(st); printf("finalize-write rc=%d\n",rc);
      }
      printf("ro-counters changes=%d total=%d\n",
        sqlite3_changes(db),sqlite3_total_changes(db));
      sqlite3_close(db);
    }
    { sqlite3*d2; rc=sqlite3_open_v2("probe_absent.db",&d2,SQLITE_OPEN_READONLY,0);
      printf("open-ro-missing rc=%d\n",rc); if(!rc)sqlite3_close(d2); }
    { sqlite3*d3; rc=sqlite3_open_v2("file:probe_ro.db?mode=ro",&d3,
        SQLITE_OPEN_READONLY|SQLITE_OPEN_URI,0);
      printf("open-ro-uri rc=%d\n",rc);
      if(!rc){ err=0; rc=sqlite3_exec(d3,"INSERT INTO t VALUES(5)",0,0,&err);
        printf("uri-write rc=%d\n",rc); sqlite3_free(err); sqlite3_close(d3); } }
    remove("probe_ro.db"); }
  return 0;}
