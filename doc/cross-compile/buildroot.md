
## How to make cross-compilation work with buildroot

1. install the build dependencies on your host PC, like: cmake, g++, clang...
   
2. install the `rust` tool: `curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh`
  
3. add the arm64 target for rust: `rustup target add aarch64-unknown-linux-gnu`
   
4. modify the arm64 target name to suit your cross-compilation tool, need to create this file `~/.cargo/config.toml`, add:
   ```
   [target.aarch64-unknown-linux-gnu]
   linker = "aarch64-buildroot-linux-gnu-gcc"
   ```
   "aarch64-buildroot-linux-gnu-gcc" is your real cross-compilation tool name

   
6. source your cross-compilation buildroot environment: `source /to/your/environment-setup`, this file should be in your buildroot folder
  
7. add the buildroot sysroot on host PC environment: `export BINDGEN_EXTRA_CLANG_ARGS="--sysroot=/to/your/sysroot"`
   
8. Change to the iceoryx2 directory
    ```console
    cd iceoryx2
    ```
9 ... 
   
9. cmake -S . -B build -DBUILD_EXAMPLES=ON -DCMAKE_INSTALL_PREFIX=../_OUTPUT -DRUST_TARGET_TRIPLET='aarch64-unknown-linux-gnu'

10. cd build

11. make -j8

12. make install

Finally, you can get the arm64 libs, include files in the `_OUTPUT` folder.
