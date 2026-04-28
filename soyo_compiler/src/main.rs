mod backend;
mod context;
mod frontend;
mod opt;

lalrpop_util::lalrpop_mod!(sysy);

fn main() {
    println!("Hello, SoyoCompiler!");
}
