use crate::framebuffer::Framebuffer;
use crate::layout::{Command, ViewLayout};
use crate::scene::{Scene, Surface, SurfaceKey};
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender, TrySendError};
use std::sync::Arc;

#[derive(Clone)]
pub struct SubscriptionStop {
    wake: SyncSender<()>,
    stopped: Arc<AtomicBool>,
}

impl SubscriptionStop {
    pub fn stop(&self) {
        self.stopped.store(true, Ordering::Release);
        let _ = self.wake.try_send(());
    }

    pub fn is_stopped(&self) -> bool {
        self.stopped.load(Ordering::Acquire)
    }
}

pub struct LayoutSubscription {
    receiver: Receiver<()>,
    stop: SubscriptionStop,
}

impl LayoutSubscription {
    pub fn split(self) -> (Receiver<()>, SubscriptionStop) {
        (self.receiver, self.stop)
    }
}

pub struct Runtime {
    scene: Scene,
    framebuffer: Framebuffer,
    layout: Arc<BTreeMap<SurfaceKey, ViewLayout>>,
    subscribers: BTreeMap<u64, SyncSender<()>>,
}

impl Runtime {
    pub fn new(framebuffer: Framebuffer) -> Runtime {
        Runtime {
            scene: Scene::new(),
            framebuffer,
            layout: Arc::new(BTreeMap::new()),
            subscribers: BTreeMap::new(),
        }
    }

    pub fn width(&self) -> usize {
        self.framebuffer.width
    }

    pub fn height(&self) -> usize {
        self.framebuffer.height
    }

    pub fn repaint(&mut self) -> Result<(), String> {
        self.framebuffer.paint(&self.scene)
    }

    pub fn commit(&mut self, key: SurfaceKey, surface: Surface) -> Result<(), String> {
        let layout_changed = self.scene.commit(key, surface)?;
        self.repaint()?;
        if layout_changed && self.refresh_layout() {
            self.publish_layout();
        }
        Ok(())
    }

    pub fn remove(&mut self, key: SurfaceKey) -> Result<(), String> {
        let layout_changed = self.scene.remove(key);
        self.repaint()?;
        if layout_changed && self.refresh_layout() {
            self.publish_layout();
        }
        Ok(())
    }

    pub fn unmap(&mut self, key: SurfaceKey) -> Result<(), String> {
        let layout_changed = self.scene.unmap(key);
        self.repaint()?;
        if layout_changed && self.refresh_layout() {
            self.publish_layout();
        }
        Ok(())
    }

    pub fn remove_client(&mut self, client: u64) -> Result<(), String> {
        let layout_changed = self.scene.remove_client(client);
        self.repaint()?;
        if layout_changed && self.refresh_layout() {
            self.publish_layout();
        }
        Ok(())
    }

    pub fn command(&mut self, command: Command) -> Result<(), String> {
        self.scene.command(command);
        self.repaint()?;
        if self.refresh_layout() {
            self.publish_layout();
        }
        Ok(())
    }

    pub fn move_pointer(&mut self, dx: i32, dy: i32) -> Result<(), String> {
        self.scene
            .move_pointer(dx, dy, self.framebuffer.width, self.framebuffer.height);
        self.repaint()
    }

    pub fn layout_snapshot(&self) -> Arc<BTreeMap<SurfaceKey, ViewLayout>> {
        Arc::clone(&self.layout)
    }

    #[cfg(test)]
    pub fn surface_size(&self, key: SurfaceKey) -> Option<(usize, usize)> {
        self.scene.surface_size(key)
    }

    pub fn subscribe(&mut self, id: u64) -> Result<LayoutSubscription, String> {
        if self.subscribers.contains_key(&id) {
            return Err(format!("layout subscriber {id} already exists"));
        }
        let (wake, receiver) = mpsc::sync_channel(1);
        wake.try_send(())
            .map_err(|_| "new layout subscription could not be primed".to_string())?;
        self.subscribers.insert(id, wake.clone());
        Ok(LayoutSubscription {
            receiver,
            stop: SubscriptionStop {
                wake,
                stopped: Arc::new(AtomicBool::new(false)),
            },
        })
    }

    pub fn unsubscribe(&mut self, id: u64) {
        self.subscribers.remove(&id);
    }

    pub fn wake_layout(&mut self, id: u64) {
        let disconnected = self
            .subscribers
            .get(&id)
            .is_some_and(|wake| matches!(wake.try_send(()), Err(TrySendError::Disconnected(()))));
        if disconnected {
            self.subscribers.remove(&id);
        }
    }

    fn refresh_layout(&mut self) -> bool {
        let next: BTreeMap<SurfaceKey, ViewLayout> = self
            .scene
            .views(self.framebuffer.width, self.framebuffer.height)
            .into_iter()
            .map(|view| (view.key, view))
            .collect();
        if self.layout.as_ref() == &next {
            return false;
        }
        self.layout = Arc::new(next);
        true
    }

    fn publish_layout(&mut self) {
        self.subscribers.retain(|_, wake| match wake.try_send(()) {
            Ok(()) | Err(TrySendError::Full(())) => true,
            Err(TrySendError::Disconnected(())) => false,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::{Axis, Direction};
    use crate::scene::SHM_XRGB8888;
    use std::fs;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    static SEQ: AtomicU64 = AtomicU64::new(0);

    struct Cleanup(PathBuf);

    impl Drop for Cleanup {
        fn drop(&mut self) {
            let _ = fs::remove_file(&self.0);
        }
    }

    fn surface(color: [u8; 4]) -> Surface {
        Surface {
            width: 100,
            height: 100,
            pixels: color.repeat(10_000),
            format: SHM_XRGB8888,
        }
    }

    #[test]
    fn commands_repaint_the_file_backed_output() {
        let path = std::env::temp_dir().join(format!(
            "td-runtime-test-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        let cleanup = Cleanup(path);
        let framebuffer = Framebuffer::test_file(&cleanup.0, 120, 80, 120 * 4).unwrap();
        let mut runtime = Runtime::new(framebuffer);
        runtime
            .commit(
                SurfaceKey {
                    client: 1,
                    object: 1,
                },
                surface([1, 2, 3, 0]),
            )
            .unwrap();
        runtime
            .commit(
                SurfaceKey {
                    client: 1,
                    object: 2,
                },
                surface([4, 5, 6, 0]),
            )
            .unwrap();
        let second_focused = fs::read(&cleanup.0).unwrap();
        runtime.command(Command::Focus(Direction::Left)).unwrap();
        let first_focused = fs::read(&cleanup.0).unwrap();
        assert_ne!(first_focused, second_focused);
    }

    #[test]
    fn layout_notifications_are_bounded_deduplicated_and_stoppable() {
        let path = std::env::temp_dir().join(format!(
            "td-runtime-subscription-test-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        let cleanup = Cleanup(path);
        let framebuffer = Framebuffer::test_file(&cleanup.0, 120, 80, 120 * 4).unwrap();
        let mut runtime = Runtime::new(framebuffer);
        let subscription = runtime.subscribe(7).unwrap();
        let (receiver, stop) = subscription.split();
        receiver.try_recv().unwrap();

        runtime
            .commit(
                SurfaceKey {
                    client: 7,
                    object: 1,
                },
                surface([1, 2, 3, 0]),
            )
            .unwrap();
        runtime
            .commit(
                SurfaceKey {
                    client: 7,
                    object: 2,
                },
                surface([4, 5, 6, 0]),
            )
            .unwrap();
        receiver.try_recv().unwrap();
        assert!(receiver.try_recv().is_err());

        let tiled = runtime.layout_snapshot();
        assert!(Arc::ptr_eq(&tiled, &runtime.layout_snapshot()));
        runtime.command(Command::SetSplit(Axis::Vertical)).unwrap();
        assert!(Arc::ptr_eq(&tiled, &runtime.layout_snapshot()));
        assert!(receiver.try_recv().is_err());
        runtime.command(Command::ToggleFullscreen).unwrap();
        let fullscreen = runtime.layout_snapshot();
        assert!(!Arc::ptr_eq(&tiled, &fullscreen));
        assert!(Arc::ptr_eq(&fullscreen, &runtime.layout_snapshot()));
        receiver.try_recv().unwrap();

        runtime.wake_layout(7);
        runtime.wake_layout(7);
        receiver.try_recv().unwrap();
        assert!(receiver.try_recv().is_err());

        stop.stop();
        receiver.recv().unwrap();
        assert!(stop.is_stopped());
        runtime.unsubscribe(7);
    }

    #[test]
    fn pixel_refreshes_and_pointer_motion_do_not_publish_layout() {
        let path = std::env::temp_dir().join(format!(
            "td-runtime-refresh-test-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        let cleanup = Cleanup(path);
        let framebuffer = Framebuffer::test_file(&cleanup.0, 120, 80, 120 * 4).unwrap();
        let mut runtime = Runtime::new(framebuffer);
        let subscription = runtime.subscribe(9).unwrap();
        let (receiver, stop) = subscription.split();
        receiver.try_recv().unwrap();
        let key = SurfaceKey {
            client: 9,
            object: 1,
        };
        runtime.commit(key, surface([1, 2, 3, 0])).unwrap();
        receiver.try_recv().unwrap();
        let layout = runtime.layout_snapshot();
        runtime.commit(key, surface([4, 5, 6, 0])).unwrap();
        runtime.move_pointer(1, 1).unwrap();
        assert!(Arc::ptr_eq(&layout, &runtime.layout_snapshot()));
        assert!(receiver.try_recv().is_err());
        stop.stop();
        runtime.unsubscribe(9);
    }
}
