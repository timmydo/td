use crate::framebuffer::Framebuffer;
use crate::layout::Command;
use crate::scene::{Scene, Surface, SurfaceKey};

pub struct Runtime {
    scene: Scene,
    framebuffer: Framebuffer,
}

impl Runtime {
    pub fn new(framebuffer: Framebuffer) -> Runtime {
        Runtime {
            scene: Scene::new(),
            framebuffer,
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
        self.scene.commit(key, surface)?;
        self.repaint()
    }

    pub fn remove(&mut self, key: SurfaceKey) -> Result<(), String> {
        self.scene.remove(key);
        self.repaint()
    }

    pub fn unmap(&mut self, key: SurfaceKey) -> Result<(), String> {
        self.scene.unmap(key);
        self.repaint()
    }

    pub fn remove_client(&mut self, client: u64) -> Result<(), String> {
        self.scene.remove_client(client);
        self.repaint()
    }

    pub fn command(&mut self, command: Command) -> Result<(), String> {
        self.scene.command(command);
        self.repaint()
    }

    pub fn move_pointer(&mut self, dx: i32, dy: i32) -> Result<(), String> {
        self.scene
            .move_pointer(dx, dy, self.framebuffer.width, self.framebuffer.height);
        self.repaint()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::Direction;
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
}
