package com.chargeguard;
import android.content.Context;
import android.hardware.camera2.CameraManager;
import android.os.Handler;
import android.os.Looper;
import java.util.HashMap;
import java.util.Map;

/** Read-only Binder callbacks. Never opens a camera or enumerates applications. */
public final class CameraEvents {
    private static String previous = "";
    private static void emit(String state) {
        if (!state.equals(previous)) { System.out.println("CGCAM1 " + state); System.out.flush(); previous=state; }
    }
    public static void main(String[] args) {
        try {
            // Parent death closes stdin. No heartbeat or periodic status requests.
            Thread lifetime=new Thread(()->{try{while(System.in.read()!=-1){}}catch(Exception ignored){}System.exit(0);},"parent-lifetime");
            lifetime.setDaemon(true);lifetime.start();
            Looper.prepareMainLooper();
            Class<?> activityThread=Class.forName("android.app.ActivityThread");
            Object thread=activityThread.getMethod("systemMain").invoke(null);
            Context system=(Context)activityThread.getMethod("getSystemContext").invoke(thread);
            Context context=system.createPackageContext("com.android.shell",0);
            CameraManager manager=(CameraManager)context.getSystemService(Context.CAMERA_SERVICE);
            Handler handler=new Handler(Looper.getMainLooper());
            Map<String,Boolean> cameras=new HashMap<>();
            for(String id:manager.getCameraIdList()) cameras.put(id,null);
            emit("unknown");
            CameraManager.AvailabilityCallback callback=new CameraManager.AvailabilityCallback(){
                private void update(String id,boolean available){
                    cameras.put(id,available);
                    if(cameras.containsValue(Boolean.FALSE)) emit("busy");
                    else if(cameras.isEmpty() || cameras.containsValue(null)) emit("unknown");
                    else emit("idle");
                }
                @Override public void onCameraAvailable(String id){update(id,true);}
                @Override public void onCameraUnavailable(String id){update(id,false);}
            };
            manager.registerAvailabilityCallback(callback,handler);
            Looper.loop();
        } catch(Throwable error) { emit("unknown");error.printStackTrace(System.err);System.exit(1); }
    }
}
