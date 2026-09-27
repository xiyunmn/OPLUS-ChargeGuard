package com.chargeguard;

import java.io.*;
import java.nio.charset.StandardCharsets;
import java.security.MessageDigest;
import java.util.*;
import java.util.zip.*;

/** Read-only DEX contracts. No APK class is loaded or initialized. */
public final class DexContract {
    static final String[] CLASSES = {
        "Lcom/oplus/thermalcontrol/config/policy/ThermalPolicy;",
        "Llb/h;", "Lcom/oplus/thermalcontrol/ThermalControlUtils;",
        "Lcom/oplus/thermalcontrol/m;", "Lpb/k;", "Lpb/b;",
        "Lcom/oplus/battery/OplusBatteryService;", "Lcom/oplus/battery/OplusBatteryApp;"
    };
    private final byte[] data;
    private final String[] strings, types, protos, fields, methods;
    private DexContract(byte[] bytes) throws Exception {
        data=bytes;
        require(bytes.length>=112 && new String(bytes,0,4,StandardCharsets.US_ASCII).equals("dex\n"));
        require(u32(32)==bytes.length && u32(36)==112 && u32(40)==0x12345678);
        strings=new String[count(56)];
        for(int i=0;i<strings.length;i++) { int[] p={u32(u32(60)+4*i)}; uleb(p); int start=p[0]; while(u8(p[0])!=0)p[0]++; strings[i]=new String(data,start,p[0]-start,StandardCharsets.UTF_8); }
        types=new String[count(64)]; for(int i=0;i<types.length;i++)types[i]=strings[u32(u32(68)+4*i)];
        protos=new String[count(72)];
        for(int i=0;i<protos.length;i++) { int p=u32(76)+12*i; protos[i]=typeList(u32(p+8))+":"+types[u32(p+4)]; }
        fields=new String[count(80)];
        for(int i=0;i<fields.length;i++){int p=u32(84)+8*i;fields[i]=types[u16(p)]+"."+strings[u32(p+4)]+":"+types[u16(p+2)];}
        methods=new String[count(88)];
        for(int i=0;i<methods.length;i++){int p=u32(92)+8*i;methods[i]=types[u16(p)]+"."+strings[u32(p+4)]+protos[u16(p+2)];}
    }
    private static void require(boolean b) { if(!b)throw new IllegalArgumentException("unsupported_dex_contract"); }
    private int u8(int p){require(p>=0&&p<data.length);return data[p]&255;}
    private int u16(int p){return u8(p)|(u8(p+1)<<8);}
    private int u32(int p){return u16(p)|(u16(p+2)<<16);}
    private int count(int p){int n=u32(p);require(n>=0&&n<=1000000);return n;}
    private int uleb(int[] p){int n=0;for(int i=0;i<5;i++){int b=u8(p[0]++);n|=(b&127)<<(7*i);if(b<128)return n;}throw new IllegalArgumentException("dex_uleb");}
    private int sleb(int[] p){int n=0,shift=0,b;do{b=u8(p[0]++);n|=(b&127)<<shift;shift+=7;require(shift<=35);}while(b>=128);return shift<32&&(b&64)!=0?n|(-1<<shift):n;}
    private String typeList(int p){if(p==0)return "[]";int n=count(p);StringBuilder s=new StringBuilder("[");for(int i=0;i<n;i++)s.append(types[u16(p+4+2*i)]).append(';');return s.append(']').toString();}
    private void value(int[] p,DataOutputStream out,int depth)throws Exception{
        require(depth<32);int header=u8(p[0]++),kind=header&31,n=(header>>>5)+1;out.writeInt(kind);
        if(kind==0x1c){int size=uleb(p);require(size<100000);out.writeInt(size);for(int i=0;i<size;i++)value(p,out,depth+1);return;}
        if(kind==0x1d){out.writeUTF(types[uleb(p)]);int size=uleb(p);require(size<100000);out.writeInt(size);for(int i=0;i<size;i++){out.writeUTF(strings[uleb(p)]);value(p,out,depth+1);}return;}
        if(kind==0x1e)return;
        if(kind==0x1f){out.writeInt(header>>>5);return;}
        long v=0;for(int i=0;i<n;i++)v|=(long)u8(p[0]++)<<(8*i);
        switch(kind){
            case 0x17:out.writeUTF(strings[(int)v]);break;
            case 0x18:out.writeUTF(types[(int)v]);break;
            case 0x19:case 0x1b:out.writeUTF(fields[(int)v]);break;
            case 0x1a:out.writeUTF(methods[(int)v]);break;
            case 0x15:out.writeUTF(protos[(int)v]);break;
            case 0:case 2:case 3:case 4:case 6:case 0x10:case 0x11:out.writeInt(n);out.writeLong(v);break;
            default:throw new IllegalArgumentException("dex_encoded_value");
        }
    }
    private int width(int p){
        int op=u16(p)&255;
        if(op==0){int tag=u16(p)>>>8;if(tag==0)return 1;if(tag==1)return 4+u16(p+2)*2;if(tag==2)return 2+u16(p+2)*4;if(tag==3){long n=(long)u16(p+2)*(u32(p+4)&0xffffffffL);require(n<=data.length);return 4+(int)((n+1)/2);}throw new IllegalArgumentException("dex_payload");}
        if(op<=9)return 1+(op-1)%3;
        if(op<=0x12)return 1;
        if(op==0x18)return 5;
        if(op==0x14||op==0x17||op==0x1b||op==0x24||op==0x25||op==0x26||op==0x2a||op==0x2b||op==0x2c)return 3;
        if(op==0x1d||op==0x1e||op==0x21||op==0x27||op==0x28)return 1;
        if(op<=0x3d)return 2;
        if(op>=0x44&&op<=0x6d)return 2;
        if((op>=0x6e&&op<=0x72)||(op>=0x74&&op<=0x78))return 3;
        if(op>=0x7b&&op<=0x8f)return 1;
        if(op>=0x90&&op<=0xaf)return 2;
        if(op>=0xb0&&op<=0xcf)return 1;
        if(op>=0xd0&&op<=0xe2)return 2;
        throw new IllegalArgumentException("dex_new_opcode");
    }
    private void code(int p,DataOutputStream out)throws Exception{
        if(p==0){out.writeInt(0);return;}out.writeInt(1);
        out.writeInt(u16(p));out.writeInt(u16(p+2));out.writeInt(u16(p+4));int tries=u16(p+6),size=count(p+12);out.writeInt(size);
        for(int i=0;i<size;){int at=p+16+2*i,w=width(at),op=u16(at)&255;require(i+w<=size);String ref=null;int index=w>1?u16(at+2):0;
            if(op==0x1a)ref=strings[index];else if(op==0x1b)ref=strings[u32(at+2)];
            else if(op==0x1c||op==0x1f||op==0x20||op==0x22||op==0x23||op==0x24||op==0x25)ref=types[index];
            else if(op>=0x52&&op<=0x6d)ref=fields[index];
            else if((op>=0x6e&&op<=0x72)||(op>=0x74&&op<=0x78))ref=methods[index];
            for(int j=0;j<w;j++)out.writeShort(ref!=null&&(j==1||(op==0x1b&&j==2))?0:u16(at+2*j));
            if(ref!=null)out.writeUTF(ref);i+=w;
        }
        out.writeInt(tries);if(tries==0)return;
        int start=p+16+2*size+(size%2)*2,base=start+tries*8;int[] cursor={base};int count=uleb(cursor);require(count<=65535);Map<Integer,byte[]> handlers=new HashMap<>();
        for(int i=0;i<count;i++){int offset=cursor[0]-base,n=sleb(cursor);require(Math.abs(n)<=65535);ByteArrayOutputStream bytes=new ByteArrayOutputStream();DataOutputStream h=new DataOutputStream(bytes);h.writeInt(n);for(int j=0;j<Math.abs(n);j++){h.writeUTF(types[uleb(cursor)]);h.writeInt(uleb(cursor));}if(n<=0)h.writeInt(uleb(cursor));handlers.put(offset,bytes.toByteArray());}
        for(int i=0;i<tries;i++){int at=start+8*i;out.writeInt(u32(at));out.writeInt(u16(at+4));byte[] h=handlers.get(u16(at+6));require(h!=null);out.writeInt(h.length);out.write(h);}
    }
    private byte[] contract(int at)throws Exception{
        ByteArrayOutputStream bytes=new ByteArrayOutputStream();DataOutputStream out=new DataOutputStream(bytes);
        out.writeUTF(types[u32(at)]);out.writeInt(u32(at+4));out.writeUTF(u32(at+8)==-1?"":types[u32(at+8)]);out.writeUTF(typeList(u32(at+12)));
        int[] p={u32(at+24)};require(p[0]>0);int sf=uleb(p),inf=uleb(p),dm=uleb(p),vm=uleb(p);int statics=u32(at+28);int[] sv={statics};int values=statics==0?0:uleb(sv);require(values<=sf);
        for(int count:new int[]{sf,inf}){out.writeInt(count);int idx=0;for(int i=0;i<count;i++){idx+=uleb(p);out.writeUTF(fields[idx]);out.writeInt(uleb(p));}}
        out.writeInt(values);for(int i=0;i<values;i++)value(sv,out,0);
        for(int count:new int[]{dm,vm}){out.writeInt(count);int idx=0;for(int i=0;i<count;i++){idx+=uleb(p);out.writeUTF(methods[idx]);out.writeInt(uleb(p));code(uleb(p),out);}}
        return MessageDigest.getInstance("SHA-256").digest(bytes.toByteArray());
    }
    static Map<String,String> inspect(String[] apks)throws Exception{
        Map<String,String> result=new TreeMap<>();Set<String> wanted=new HashSet<>(Arrays.asList(CLASSES));long total=0;
        for(String apk:apks)try(ZipFile zip=new ZipFile(apk)){
            Enumeration<? extends ZipEntry> entries=zip.entries();int dexCount=0;
            while(entries.hasMoreElements()) {ZipEntry entry=entries.nextElement();if(!entry.getName().matches("classes([2-9]|[1-9][0-9]+)?\\.dex"))continue;require(++dexCount<=32&&entry.getSize()>0&&entry.getSize()<=64*1024*1024);total+=entry.getSize();require(total<=128*1024*1024);
                ByteArrayOutputStream bytes=new ByteArrayOutputStream();try(InputStream in=zip.getInputStream(entry)){byte[] buf=new byte[16384];int n;while((n=in.read(buf))!=-1){require(bytes.size()+n<=entry.getSize());bytes.write(buf,0,n);}}DexContract dex=new DexContract(bytes.toByteArray());
                int count=dex.count(96),offset=dex.u32(100);for(int i=0;i<count;i++){int at=offset+32*i;String name=dex.types[dex.u32(at)];if(!wanted.contains(name))continue;StringBuilder hex=new StringBuilder();for(byte b:dex.contract(at))hex.append(String.format(Locale.ROOT,"%02x",b&255));require(result.put(name,hex.toString())==null);}
            }
        }
        require(result.keySet().equals(wanted));return result;
    }
    public static void main(String[] args)throws Exception{for(Map.Entry<String,String> e:inspect(args).entrySet())System.out.println(e.getKey()+" "+e.getValue());}
}
