package io.nairobi.app;

import android.app.NativeActivity;
import android.os.Build;
import android.os.Bundle;
import android.view.View;
import android.view.WindowManager;
import android.window.OnBackInvokedCallback;
import android.window.OnBackInvokedDispatcher;

/**
 * Thin NativeActivity subclass. All UI lives in Rust (Slint); this class
 * exists to forward runtime-permission results to native code (plain
 * NativeActivity cannot) and to expose the current activity instance to
 * {@link LocationBridge}.
 *
 * Note: the Rust side must never pass a context across JNI itself — the
 * android-activity glue only publishes the *Application* context, which is
 * not an Activity. The bridge always resolves the live activity here via
 * {@link #current()}.
 */
public class MainActivity extends NativeActivity {

    private static volatile MainActivity sInstance;

    /** Registered on API 33+ so the predictive back gesture routes to Rust. */
    private OnBackInvokedCallback backCallback;

    /** The currently alive activity, or null between destroy and recreate. */
    static MainActivity current() {
        return sInstance;
    }

    @Override
    protected void onCreate(Bundle savedInstanceState) {
        sInstance = this;
        super.onCreate(savedInstanceState);
        // Keep the screen on while the app is in the foreground: an active ride
        // (waiting for a match, or navigating) is typically glanced at, not
        // continuously interacted with.
        getWindow().addFlags(WindowManager.LayoutParams.FLAG_KEEP_SCREEN_ON);
        setupEdgeToEdge();
        registerBackHandler();
    }

    /**
     * Intercept the system back gesture/button so it walks back through the
     * in-app screens instead of finishing the activity (the default for a
     * NativeActivity, which would close the app on the first back). The press is
     * forwarded to Rust, which navigates — and only calls {@link
     * LocationBridge#finishActivity} to actually exit when already on Home.
     *
     * On API 33+ we register an {@link OnBackInvokedCallback} (this is also what
     * drives the predictive-back animation, enabled via the manifest); on older
     * releases the deprecated {@link #onBackPressed} override below handles it.
     */
    private void registerBackHandler() {
        if (Build.VERSION.SDK_INT >= 33) {
            backCallback = LocationBridge::dispatchBack;
            getOnBackInvokedDispatcher().registerOnBackInvokedCallback(
                    OnBackInvokedDispatcher.PRIORITY_DEFAULT, backCallback);
        }
    }

    @Override
    @SuppressWarnings("deprecation") // legacy path for API < 33 (and when the
    // OnBackInvokedCallback is not active); we deliberately do not call super so
    // the activity is never finished here — Rust decides via finishActivity().
    public void onBackPressed() {
        LocationBridge.dispatchBack();
    }

    @Override
    protected void onResume() {
        super.onResume();
        sInstance = this;
    }

    /**
     * Lay the window out edge-to-edge so the Slint UI owns the full window,
     * including the area behind the status and gesture/navigation bars.
     *
     * A NativeActivity's surface always fills the whole window, so it has
     * always extended under the system bars; that overlap used to be masked by
     * the opaque status/navigation bar colors set in the theme. Android 15+
     * ignores those colors and forces transparent, edge-to-edge bars, exposing
     * the overlap. The Slint Android backend reads the window insets itself and
     * exposes them as the Window's `safe-area-insets` (logical pixels, updated
     * on configuration changes), which the UI uses to pad around the bars — so
     * no manual inset plumbing is needed here, only edge-to-edge layout.
     */
    private void setupEdgeToEdge() {
        if (Build.VERSION.SDK_INT >= 30) {
            getWindow().setDecorFitsSystemWindows(false);
        } else {
            getWindow().getDecorView().setSystemUiVisibility(
                    View.SYSTEM_UI_FLAG_LAYOUT_STABLE
                            | View.SYSTEM_UI_FLAG_LAYOUT_FULLSCREEN
                            | View.SYSTEM_UI_FLAG_LAYOUT_HIDE_NAVIGATION);
        }
    }

    @Override
    protected void onDestroy() {
        if (Build.VERSION.SDK_INT >= 33 && backCallback != null) {
            getOnBackInvokedDispatcher().unregisterOnBackInvokedCallback(backCallback);
            backCallback = null;
        }
        if (sInstance == this) {
            sInstance = null;
        }
        super.onDestroy();
    }

    @Override
    public void onRequestPermissionsResult(int requestCode, String[] permissions, int[] grantResults) {
        super.onRequestPermissionsResult(requestCode, permissions, grantResults);
        LocationBridge.handlePermissionResult(requestCode, permissions, grantResults);
    }
}
