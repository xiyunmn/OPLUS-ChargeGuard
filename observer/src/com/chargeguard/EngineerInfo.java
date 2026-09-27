package com.chargeguard;

import android.content.*;
import android.content.pm.*;
import android.os.Looper;
import org.json.JSONObject;
import java.util.*;

/** Inspect PackageManager and DEX bytes without executing Battery application code. */
public final class EngineerInfo {
    private static final String[] EXPECTED = {
        "d65566eb822b9804e9b6a03aec60085110427715c6f53f7a98d4536f5b987b95",
        "e3ad4e00fc73528bd84c2c832c7ab0c29892479b4a620921e1b42b568801486c",
        "f36e581ca7f297e7de44493659acae8b5b36c8ec0d7a43ca05b29cd38514b30a",
        "98abf82b83e363c81448a492df7b108029e87ea09ce71583dfcf5cefe3e3c0b7",
        "dd2d6b987a92d410c55574d8a415673f859c075ac0e189d2a2bbb10933d8fdd8",
        "d67b80fc1f0e86b7204d799ff88a44368ca4e9197a0159c2489077e75dc992ad",
        "3f4758b79617c4fe4c8116bf7467fbf70a8adebe70e84ae9fe01791d32c00102",
        "7cc4e7b425178d710d3dc11a4c087d403ffea9e432921034a363001a892d5486"
    };
    public static void main(String[] args) {
        try {
            Looper.prepareMainLooper();
            Class<?> at=Class.forName("android.app.ActivityThread");Object thread=at.getMethod("systemMain").invoke(null);
            Context context=(Context)at.getMethod("getSystemContext").invoke(thread);
            PackageManager pm=context.getPackageManager();
            PackageInfo pkg=pm.getPackageInfo("com.oplus.battery",PackageManager.GET_SERVICES);
            ApplicationInfo app=pkg.applicationInfo;ServiceInfo service=null;
            if(pkg.services!=null)for(ServiceInfo s:pkg.services)if(s.name.equals("com.oplus.battery.OplusBatteryService"))service=s;
            if(app==null||app.uid!=1000||!app.enabled||(app.flags&(ApplicationInfo.FLAG_SYSTEM|ApplicationInfo.FLAG_UPDATED_SYSTEM_APP))==0||service==null||!service.enabled||!"com.oplus.athena".equals(service.processName))throw new IllegalStateException("policy_battery_component_unsupported");
            List<String> paths=new ArrayList<>();paths.add(app.sourceDir);if(app.splitSourceDirs!=null)Collections.addAll(paths,app.splitSourceDirs);
            Map<String,String> contracts=DexContract.inspect(paths.toArray(new String[0]));
            for(int i=0;i<DexContract.CLASSES.length;i++)if(!EXPECTED[i].equals(contracts.get(DexContract.CLASSES[i])))throw new IllegalStateException("policy_battery_code_contract_changed:"+DexContract.CLASSES[i]);
            PackageInfo after=pm.getPackageInfo(pkg.packageName,0);
            if(after.lastUpdateTime!=pkg.lastUpdateTime||!after.applicationInfo.sourceDir.equals(app.sourceDir)||!Arrays.equals(after.applicationInfo.splitSourceDirs,app.splitSourceDirs))throw new IllegalStateException("policy_battery_changed_during_check");
            System.out.println(new JSONObject().put("api",1).put("compatible",true).put("process",service.processName).put("uid",app.uid).put("apk",app.sourceDir));
        } catch(Throwable error) {
            try { System.out.println(new JSONObject().put("api",1).put("compatible",false).put("error",String.valueOf(error.getMessage()))); }
            catch(Exception ignored) { System.out.println("{\"api\":1,\"compatible\":false}"); }
        }
    }
}
