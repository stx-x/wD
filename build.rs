fn main() {
    slint_build::compile("ui/app.slint").expect("Slint UI 编译失败");
    println!("cargo:rerun-if-changed=ui/app.slint");
}
