use crate::framebuffer::Framebuffer;
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

    pub fn remove_client(&mut self, client: u64) -> Result<(), String> {
        self.scene.remove_client(client);
        self.repaint()
    }

    pub fn focus_left(&mut self) -> Result<(), String> {
        self.scene.focus_left();
        self.repaint()
    }

    pub fn focus_right(&mut self) -> Result<(), String> {
        self.scene.focus_right();
        self.repaint()
    }

    pub fn move_pointer(&mut self, dx: i32, dy: i32) -> Result<(), String> {
        self.scene
            .move_pointer(dx, dy, self.framebuffer.width, self.framebuffer.height);
        self.repaint()
    }
}
