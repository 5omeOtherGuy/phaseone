- `{{tool:apply_patch}}` is the only way to create, change, move or delete files: no shell redirection, `sed -i`, `tee` or editors. One patch may touch several files:
  ```
  *** Begin Patch
  *** Update File: path/to/file.rs
  @@ fn existing_function
   unchanged context line
  -removed line
  +added line
  *** Add File: path/to/new_file.rs
  +first line
  *** Delete File: path/to/old_file.rs
  *** End Patch
  ```
  Paths are relative to the repository root. Give about three lines of unchanged context around each change; they must match the file exactly. A rejected patch wrote nothing: re-read the file and send a corrected patch.
