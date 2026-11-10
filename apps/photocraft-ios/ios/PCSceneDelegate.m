#import <UIKit/UIKit.h>
#import <QuartzCore/QuartzCore.h>
#import <objc/runtime.h>

static UIWindowScene *PCActiveScene;
static NSMutableArray<UIWindow *> *PCPendingWindows;

@interface UIWindow (PCSceneAttachment)
- (instancetype)pc_initWithFrame:(CGRect)frame;
@end

@implementation UIWindow (PCSceneAttachment)

+ (void)load {
    PCPendingWindows = [[NSMutableArray alloc] init];
    Method original = class_getInstanceMethod(self, @selector(initWithFrame:));
    Method replacement = class_getInstanceMethod(self, @selector(pc_initWithFrame:));
    if (original != NULL && replacement != NULL) {
        method_exchangeImplementations(original, replacement);
    }
}

- (instancetype)pc_initWithFrame:(CGRect)frame {
    UIWindow *window = [self pc_initWithFrame:frame];
    if ([NSStringFromClass(window.class) isEqualToString:@"WinitUIWindow"]) {
        if (PCActiveScene != nil) {
            window.windowScene = PCActiveScene;
        } else {
            [PCPendingWindows addObject:window];
        }
    }
    return window;
}

static void PCAttachWindow(UIWindow *window, UIWindowScene *scene) {
    window.windowScene = scene;
    [window.rootViewController.view setNeedsLayout];
    [window makeKeyAndVisible];
}

/// The attach above can land in UIKit's transitional geometry (the scene's bounds are still
/// settling), leaving the root view — and the Metal layer with it — sized for the transitional
/// window: the render is then scaled anisotropically into the real screen (a 6.4% vertical
/// stretch was measured on the iPad Pro 11 27.0 simulator), so taps land progressively below
/// their targets. Once the scene has settled (the delayed retries), pinning the view to the
/// window's real bounds and laying it out sends winit the true size and the layer follows.
static void PCSyncViewToWindow(UIWindow *window) {
    UIViewController *vc = window.rootViewController;
    if (vc == nil) {
        return;
    }
    UIView *view = vc.view;
    if (view == nil) {
        return;
    }
    if (!CGRectEqualToRect(view.frame, window.bounds)) {
        view.frame = window.bounds;
        [view setNeedsLayout];
        [view layoutIfNeeded];
        NSLog(@"PCSD synced view to window bounds %@ (was stale)", NSStringFromCGRect(window.bounds));
    }
    // wgpu adds the CAMetalLayer it renders into as a sublayer of the view, sized to the view
    // at surface creation — and sublayers do not track the view. A layer created during
    // UIKit's transitional geometry (the scene-attach race) then presents the render
    // anisotropically scaled into the settled view: a 6.4% vertical stretch was measured
    // (layer 1210x887pt in an 834pt-tall view), which sent taps progressively below their
    // targets. Pin every metal sublayer to the view's real bounds.
    Class metal = NSClassFromString(@"CAMetalLayer");
    for (CALayer *sub in view.layer.sublayers) {
        if (metal == nil || ![sub isKindOfClass:metal] || CGRectEqualToRect(sub.frame, view.bounds)) {
            continue;
        }
        sub.frame = view.bounds;
        NSLog(@"PCSD pinned metal sublayer to view bounds %@ (was %@)", NSStringFromCGRect(view.bounds), NSStringFromCGRect(sub.frame));
    }
}

@end

// Scene-lifecycle adoption for iOS 26+/27: the OS traps at launch when an app neither adopts
// UIScene nor is pre-scene legacy (UIApplicationEvaluateRuntimeIssueForNoSceneLifecycleAdoption,
// EXC_BREAKPOINT right after launch). Adoption itself is declarative: an empty UIResponder
// conforming to UIWindowSceneDelegate, named in Info.plist's UIApplicationSceneManifest.
//
// The window is driven by winit through the classic path: it creates its UIWindow during
// application-did-finish-launching — possibly BEFORE this scene connects. A window left without
// a scene gets no layout pass and renders nothing (egui's viewport stays its 10000x10000
// default: the black screen). So this delegate attaches every scene-less window to the scene
// connecting now. The window's FRAME is deliberately never set here: at willConnect the scene's
// geometry is a transitional half-initialized value (834x449 was observed on the iPad Pro 11
// 27.0 simulator) and UIKit sizes a scene's windows itself on the next layout pass — setting
// the frame manually cemented the transitional size (content offset into the middle of the
// screen) and stuck, because later retries skip already-attached windows.
//
// NSLog diagnostics (visible via `xcrun simctl spawn <udid> log show --predicate
// 'process == "PhotoCraft"'`): window counts at each hook.
@interface PCSceneDelegate : UIResponder <UIWindowSceneDelegate>
@end

__attribute__((visibility("default"))) void pc_log_geometry(void) {
    for (UIWindow *window in UIApplication.sharedApplication.windows) {
        if (![window isKindOfClass:[UIWindow class]]) {
            continue;
        }
        UIView *view = window.rootViewController.view;
        CALayer *layer = view.layer;
        NSString *drawable = @"n/a";
        if ([layer isKindOfClass:NSClassFromString(@"CAMetalLayer")]) {
#pragma clang diagnostic push
#pragma clang diagnostic ignored "-Wdeprecated-declarations"
            drawable = NSStringFromCGSize([(CAMetalLayer *)layer drawableSize]);
#pragma clang diagnostic pop
        }
        NSLog(@"PCGEOM window=%@ view.frame=%@ layer.bounds=%@ contentsScale=%.3f drawable=%@ screen=%@ scene=%@",
              NSStringFromCGRect(window.bounds), NSStringFromCGRect(view.frame),
              NSStringFromCGRect(layer.bounds), layer.contentsScale, drawable,
              NSStringFromCGRect(window.screen.bounds),
              NSStringFromCGRect(((UIWindowScene *)window.windowScene).coordinateSpace.bounds));
        // The full view/layer tree: where the metal layer actually lives and what size it is.
        void (^dump)(UIView *, int) = ^(UIView *v, int depth) {
            NSString *pad = [@"" stringByPaddingToLength:depth * 2 withString:@" " startingAtIndex:0];
            NSString *extra = @"";
            if ([v.layer isKindOfClass:NSClassFromString(@"CAMetalLayer")]) {
                extra = [NSString stringWithFormat:@" METAL frame=%@ bounds=%@ drawable=%@ scale=%.2f",
                         NSStringFromCGRect(v.layer.frame), NSStringFromCGRect(v.layer.bounds),
                         NSStringFromCGSize([(CAMetalLayer *)v.layer drawableSize]), v.layer.contentsScale];
            }
            NSLog(@"PCGEOM %@%@ (%@) frame=%@ layer.frame=%@%@", pad, NSStringFromClass(v.class),
                  NSStringFromCGRect(v.frame), NSStringFromCGRect(v.layer.frame), extra);
            for (UIView *sub in v.subviews) {
                dump(sub, depth + 1);
            }
            for (CALayer *sl in v.layer.sublayers) {
                NSString *tag = [sl isKindOfClass:NSClassFromString(@"CAMetalLayer")]
                    ? [NSString stringWithFormat:@" METAL-BOUND? frame=%@ drawable=%@ scale=%.2f", NSStringFromCGRect(sl.frame), NSStringFromCGSize([(CAMetalLayer *)sl drawableSize]), sl.contentsScale]
                    : @"";
                NSLog(@"PCGEOM %@  [layer] %@ frame=%@%@", pad, NSStringFromClass(sl.class), NSStringFromCGRect(sl.frame), tag);
            }
        };
        dump(view, 0);
        break;
    }
}

@implementation PCSceneDelegate

- (void)attachWindowsToScene:(UIWindowScene *)scene cause:(NSString *)cause {
    NSUInteger attached = 0;
    PCActiveScene = scene;
    for (UIWindow *window in PCPendingWindows) {
        PCAttachWindow(window, scene);
        attached++;
    }
    [PCPendingWindows removeAllObjects];
    NSLog(@"PCSD %@: scene=%p attachedPending=%lu", cause, scene, (unsigned long)attached);
}

- (void)scene:(UIScene *)scene willConnectToSession:(UISceneSession *)session options:(UISceneConnectionOptions *)connectionOptions {
    NSLog(@"PCSD willConnect begin: scene=%p", scene);
    if (![scene isKindOfClass:[UIWindowScene class]]) {
        NSLog(@"PCSD willConnect: not a window scene, ignoring");
        return;
    }
    [self attachWindowsToScene:(UIWindowScene *)scene cause:@"willConnect"];
    // A window created after this hook (winit's is created inside didFinishLaunching, whose
    // order against scene connection is not guaranteed across OS versions) needs another pass:
    // repeat on the main queue a few times over the first seconds of the run.
    for (NSNumber *delayValue in @[@0.5, @2.0, @5.0]) {
        double delay = delayValue.doubleValue;
        UIWindowScene *captured = (UIWindowScene *)scene;
        dispatch_after(dispatch_time(DISPATCH_TIME_NOW, (int64_t)(delay * NSEC_PER_SEC)),
                       dispatch_get_main_queue(), ^{
            [self attachWindowsToScene:captured
                                cause:[NSString stringWithFormat:@"willConnect+%.1fs", delay]];
            for (UIWindow *window in UIApplication.sharedApplication.windows) {
                if ([window isKindOfClass:[UIWindow class]]) {
                    PCSyncViewToWindow(window);
                }
            }
        });
    }
}

// Rotation, Split View and Stage Manager resize the window; the metal sublayer must be
// re-pinned to the new bounds (see PCSyncViewToWindow).
- (void)windowScene:(UIWindowScene *)windowScene didUpdateCoordinateSpace:(id<UICoordinateSpace>)previousCoordinateSpace interfaceOrientation:(UIInterfaceOrientation)previousInterfaceOrientation traitCollection:(UITraitCollection *)previousTraitCollection {
    for (UIWindow *window in windowScene.windows) {
        if ([window isKindOfClass:[UIWindow class]]) {
            PCSyncViewToWindow(window);
        }
    }
}

// A scene that activates after connecting (return from background, stage manager) may again
// find a window that arrived without one; the same attachment covers it.
- (void)sceneDidBecomeActive:(UIScene *)scene {
    NSLog(@"PCSD sceneDidBecomeActive: scene=%p", scene);
    if (![scene isKindOfClass:[UIWindowScene class]]) {
        return;
    }
    [self attachWindowsToScene:(UIWindowScene *)scene cause:@"didBecomeActive"];
    for (UIWindow *window in UIApplication.sharedApplication.windows) {
        // The deprecated enumeration has been observed to hand out scenes as well on iOS 27;
        // only actual windows have a root view controller to sync.
        if ([window isKindOfClass:[UIWindow class]]) {
            PCSyncViewToWindow(window);
        }
    }
}

@end
